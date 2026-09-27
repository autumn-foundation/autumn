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
//! * [`wal`](crate::replication::wal) — `SQLite`'s WAL format: header, frame
//!   checksum chain, commit boundaries. The byte-level truth everything else
//!   depends on.
//! * [`segment`](crate::replication::segment) — the destination's object
//!   namespace and the self-describing segment payload.
//! * [`destination`](crate::replication::destination) — the object-store seam,
//!   plus the filesystem implementation; `s3` is the S3-compatible one.
//! * [`engine`](crate::replication::engine) — the replication loop and the
//!   checkpoint interlock that makes it safe.
//! * [`restore`](crate::replication::restore) — point-in-time restore, and the
//!   verification that refuses a replica rather than handing `SQLite` a damaged
//!   WAL.
//! * [`status`](crate::replication::status) — what the operator sees: lag,
//!   generation, verification, errors.
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
use std::path::{Path, PathBuf};
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
        || uri_asks_for_memory(&target)
    {
        return None;
    }
    // A `file:` target is a URI, and `SQLite` reads it as one: the query string
    // carries options rather than path, an empty or `localhost` authority is
    // dropped, and the filename is percent-decoded. diesel opens with
    // `SQLITE_OPEN_URI`, so `file:app%20data.db` really does name `app data.db` —
    // returning the raw text would point every caller at a file that does not
    // exist. Every other spelling is handed to `sqlite3_open` as a literal path
    // and is returned verbatim.
    let Some(uri) = target.strip_prefix("file:") else {
        return Some(PathBuf::from(target));
    };
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    // `file://<authority>/path`: SQLite accepts only an empty or `localhost`
    // authority, and the path starts at the `/` that ends it.
    let path = path.strip_prefix("//").map_or(path, |rest| {
        rest.find('/')
            .map_or("", |slash| rest.get(slash..).unwrap_or_default())
    });
    let decoded = percent_decode(path);
    // `SQLite` hands the decoded name to a NUL-terminated C API, so an encoded NUL
    // ends the filename: `file:app%00ignored.db` opens `app`. Keeping the tail
    // would name a path the OS rejects outright — a backup would report the live
    // database missing, and a deploy would carry a NUL into a remote command.
    let decoded = decoded
        .split(|byte| *byte == 0)
        .next()
        .unwrap_or_default()
        .to_vec();
    // Re-check for the in-memory token AFTER decoding: the checks above see the
    // raw target, so `file:%3Amemory%3A` and `file::memory:#fragment` both reach
    // here as `:memory:`. Returning that as a path would have the deploy link a
    // persistence file the app never opens.
    if decoded.is_empty() || decoded == b":memory:" {
        return None;
    }
    decoded_path(decoded)
}

/// Turn decoded `SQLite` URI bytes into a path.
///
/// `SQLite` percent-decodes to BYTES and hands them to `open(2)`, so
/// `file:app%FF.db` names `app\xFF.db` — a real filename on a POSIX host and not
/// valid UTF-8. A lossy conversion would name `app\u{FFFD}.db` instead: a
/// different file, which is how a backup comes to report a live database missing.
#[cfg(unix)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the non-unix sibling genuinely refuses; one signature keeps the caller single"
)]
fn decoded_path(bytes: Vec<u8>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;

    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// Non-POSIX hosts name files in UTF-16, so a non-UTF-8 byte sequence names no
/// file at all. Refuse it rather than guess with a replacement character.
#[cfg(not(unix))]
fn decoded_path(bytes: Vec<u8>) -> Option<PathBuf> {
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

/// Whether a `file:` URI asks for an in-memory database through its QUERY.
///
/// A substring test would refuse a durable file that merely happens to be named
/// `mode=memory.db`, which `SQLite` opens as an ordinary file. Only `mode=memory`
/// standing alone as a query parameter means in-memory.
fn uri_asks_for_memory(target: &str) -> bool {
    let Some(uri) = target.strip_prefix("file:") else {
        return false;
    };
    // A fragment ends the query.
    let uri = uri.split('#').next().unwrap_or(uri);
    let Some((_, query)) = uri.split_once('?') else {
        return false;
    };
    // `&` only: SQLite does not treat `;` as a separator, so in
    // `file:app.db?x=1;mode=memory` the semicolon is part of `x`'s value and the
    // target is the durable `app.db`.
    query.split('&').any(|parameter| parameter == "mode=memory")
}

/// The string to hand `sqlite3_open` for an already-resolved database FILE.
///
/// diesel opens with `SQLITE_OPEN_URI`, so a filename that itself begins with
/// `file:` is re-read as a URI and names a DIFFERENT database — which
/// `sqlite3_open` then creates, empty. A real file named `file:prod.db` (spelled
/// `file:file%3Aprod.db` in config) would otherwise be backed up as a fresh empty
/// `prod.db`, reported as a success. A `./` prefix makes it an unambiguous
/// relative path; an absolute path cannot begin with `file:` and is untouched.
///
/// Returns `None` for a path no `&str` connection string can carry.
#[must_use]
pub fn connection_string(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    Some(if text.starts_with("file:") {
        format!("./{text}")
    } else {
        text.to_owned()
    })
}

/// Percent-decode a `SQLite` URI path the way `sqlite3_open` does.
///
/// `%HH` with two hex digits becomes that byte; anything else — a stray `%`, a
/// truncated or non-hex escape — is left alone, matching `SQLite`'s own lenient
/// parser. Returns BYTES, not a `String`: the result is a filename, and a
/// filename need not be valid UTF-8.
fn percent_decode(path: &str) -> Vec<u8> {
    let bytes = path.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let decoded = if byte == b'%' {
            bytes
                .get(index.saturating_add(1))
                .zip(bytes.get(index.saturating_add(2)))
                .and_then(|(high, low)| {
                    let high = char::from(*high).to_digit(16)?;
                    let low = char::from(*low).to_digit(16)?;
                    u8::try_from(high.saturating_mul(16).saturating_add(low)).ok()
                })
        } else {
            None
        };
        if let Some(value) = decoded {
            out.push(value);
            index = index.saturating_add(3);
        } else {
            out.push(byte);
            index = index.saturating_add(1);
        }
    }
    out
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
/// HTTP client (see the `s3` module).
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
    // Every object this replicator writes hangs off the derived root, and every
    // destination validates the keys it is handed. A prefix that makes the root
    // unusable — `../archive`, `archive:sqlite` — therefore passes config
    // validation and then fails *every* upload at runtime, with the app already
    // serving and auto-checkpointing already disabled, so the WAL grows without
    // bound while the operator is told replication is on. Fail closed here
    // instead: the check is the destination's own, so it cannot drift from what
    // the uploads will accept.
    let root = segment::root_prefix(config.prefix.as_deref(), profile);
    if let Err(e) = destination::validate_key(&root) {
        return Err(SetupError::Config {
            errors: vec![format!(
                "[replication] prefix {:?} makes an unusable destination root {root:?}: {e}",
                config.prefix.as_deref().unwrap_or_default()
            )],
        });
    }
    let settings = ReplicationSettings {
        database_path,
        root,
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

    /// A `file:` target is a URI. diesel opens with `SQLITE_OPEN_URI`, so the
    /// filename is percent-decoded and the authority is dropped — the resolver
    /// must name the file `SQLite` really opens, not the raw text.
    #[test]
    fn database_file_decodes_a_file_uri_the_way_sqlite_does() {
        for (url, expected) in [
            ("file:app%20data.db", "app data.db"),
            (
                "file:/srv/my%20app/db%2Efile?mode=rwc",
                "/srv/my app/db.file",
            ),
            // An empty or `localhost` authority is dropped; the path starts at
            // the `/` that ends it.
            ("file:///srv/app.db", "/srv/app.db"),
            ("file://localhost/srv/app.db", "/srv/app.db"),
            // A multi-byte UTF-8 escape reassembles.
            ("file:caf%C3%A9.db", "café.db"),
            // A stray or truncated escape is left alone, like SQLite's own parser.
            ("file:100%.db", "100%.db"),
            ("file:a%2Fb.db", "a/b.db"),
            ("file:a%zz.db", "a%zz.db"),
            ("file:trailing%", "trailing%"),
            // An encoded NUL ends the filename, as it does for SQLite's own
            // NUL-terminated open.
            ("file:app%00ignored.db", "app"),
            ("file:/srv/app%00.db?mode=rwc", "/srv/app"),
        ] {
            assert_eq!(database_file(url), Some(PathBuf::from(expected)), "{url}");
        }
        // Every other spelling is a literal path handed to `sqlite3_open`, so a
        // `%` in it stays a `%`.
        assert_eq!(
            database_file("sqlite://app%20data.db"),
            Some(PathBuf::from("app%20data.db"))
        );
    }

    /// `SQLite` decodes a URI filename to BYTES and hands them to `open(2)`, so
    /// `file:app%FF.db` names a real POSIX file whose name is not valid UTF-8.
    /// A lossy conversion would name a DIFFERENT file (`app\u{FFFD}.db`), which
    /// is how a backup comes to report a live database missing.
    #[cfg(unix)]
    #[test]
    fn database_file_keeps_non_utf8_bytes_a_file_uri_decodes_to() {
        use std::os::unix::ffi::OsStrExt as _;

        let resolved = database_file("file:app%FF.db").expect("names a file");
        assert_eq!(
            resolved.as_os_str().as_bytes(),
            b"app\xFF.db",
            "the decoded bytes must survive, not become a replacement character"
        );
        assert!(
            resolved.to_str().is_none(),
            "this path is deliberately not valid UTF-8"
        );
    }

    /// A filename that merely CONTAINS `mode=memory` is a durable file. Only the
    /// URI query parameter asks for an in-memory database.
    #[test]
    fn database_file_reads_mode_memory_from_the_query_not_the_filename() {
        for durable in [
            "sqlite://mode=memory.db",
            "sqlite:///var/lib/mode=memory.db",
            "file:mode=memory.db",
        ] {
            assert!(
                database_file(durable).is_some(),
                "{durable} names a durable file"
            );
        }
        for memory in [
            "file:app?mode=memory",
            "file:app?cache=shared&mode=memory",
            "file:app?mode=memory#frag",
        ] {
            assert_eq!(database_file(memory), None, "{memory} is in-memory");
        }
        // A name that is nothing but a NUL names no file at all.
        assert_eq!(database_file("file:%00"), None);
        // The in-memory token can hide behind percent-encoding or a fragment; a
        // raw-text check runs before decoding, so re-check after it.
        for memory in [
            "file:%3Amemory%3A",
            "file::memory:#fragment",
            "file:%3amemory%3a",
        ] {
            assert_eq!(database_file(memory), None, "{memory} is in-memory");
        }
        // A parameter that merely starts with it is a different parameter.
        assert!(database_file("file:app?mode=memoryx").is_some());
        // `&` is SQLite's only query separator, so a `;` stays part of a value.
        assert!(
            database_file("file:app.db?x=1;mode=memory").is_some(),
            "a semicolon does not separate parameters, so this names the durable app.db"
        );
    }

    /// diesel opens with `SQLITE_OPEN_URI`, so a filename that itself begins with
    /// `file:` would be re-read as a URI — naming, and CREATING, a different
    /// database. A backup would then snapshot an empty file and report success.
    #[test]
    fn connection_string_keeps_a_file_prefixed_name_from_becoming_a_uri() {
        assert_eq!(
            connection_string(Path::new("file:prod.db")).as_deref(),
            Some("./file:prod.db")
        );
        // An absolute path cannot begin with `file:`, so it is untouched.
        assert_eq!(
            connection_string(Path::new("/srv/file:prod.db")).as_deref(),
            Some("/srv/file:prod.db")
        );
        assert_eq!(
            connection_string(Path::new("app.db")).as_deref(),
            Some("app.db")
        );
    }

    /// End to end: the config spelling for a file literally named `file:prod.db`
    /// must reach THAT file, not a fresh empty `prod.db`.
    #[test]
    fn a_file_named_like_a_uri_round_trips_through_the_resolver() {
        let resolved = database_file("file:file%3Aprod.db?mode=rwc").expect("names a file");
        assert_eq!(resolved, PathBuf::from("file:prod.db"));
        assert_eq!(
            connection_string(&resolved).as_deref(),
            Some("./file:prod.db"),
            "handing the decoded name straight back to sqlite3_open would create \
             a different, empty database"
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

        // An explicit override, within the RPO it has to meet — the pair the
        // validator accepts. (Longer than the RPO is refused; see
        // `a_sync_interval_longer_than_the_rpo_is_refused`.)
        let explicit = ReplicationConfig {
            rpo_secs: 60,
            sync_interval_secs: Some(30),
            ..ReplicationConfig::default()
        };
        assert_eq!(explicit.sync_interval(), Duration::from_secs(30));
    }

    /// An explicit ship interval must be able to meet the RPO it sits next to.
    ///
    /// `rpo_secs = 10` with `sync_interval_secs = 60` ships once a minute and
    /// can lose nearly a minute of committed writes, while every surface still
    /// promises ten seconds. The pair is refused rather than silently making the
    /// stricter-looking number the wrong one.
    #[test]
    fn a_sync_interval_longer_than_the_rpo_is_refused() {
        let bad = ReplicationConfig {
            enabled: true,
            rpo_secs: 10,
            sync_interval_secs: Some(60),
            path: Some("/tmp/replica".to_owned()),
            ..ReplicationConfig::default()
        };
        let errors = bad.validation_errors();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("sync_interval_secs") && e.contains("rpo_secs")),
            "the pair must be rejected: {errors:?}"
        );

        // Equal is fine: shipping exactly at the objective still meets it, and
        // so is the default, which derives the interval from the RPO.
        let tight = ReplicationConfig {
            enabled: true,
            rpo_secs: 10,
            sync_interval_secs: Some(10),
            path: Some("/tmp/replica".to_owned()),
            ..ReplicationConfig::default()
        };
        assert!(
            !tight
                .validation_errors()
                .iter()
                .any(|e| e.contains("sync_interval_secs")),
            "an interval equal to the RPO is valid"
        );
    }

    #[test]
    fn verification_can_be_switched_off() {
        let mut config = ReplicationConfig::default();
        assert_eq!(config.verify_interval(), Some(Duration::from_secs(21_600)));
        config.verify_interval_secs = 0;
        assert_eq!(config.verify_interval(), None);
    }
}
