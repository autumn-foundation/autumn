//! Writing capsules to disk, and reading them back.
//!
//! Capsules land in a plain directory of JSON files (`tmp/autumn-capsules` by
//! default, project-relative like the maintenance flag file). Because the
//! contents are real production request data, the writer is deliberately
//! conservative: owner-only permissions on unix, a temp-then-rename so a
//! reader never sees a half-written file, and an oldest-first prune so an
//! error storm cannot fill a disk.
//!
//! Retention deliberately errs towards keeping too much. `max_capsules` is
//! clamped to at least one, pruning runs *before* the new capsule is written
//! (so the capsule this request just produced can never be the one deleted to
//! make room for it), and a capsule written within [`PRUNE_GRACE`] is spared
//! even when it is over the cap — an error storm hands several capsules to
//! reporters at once, and a path in an already-dispatched
//! [`ErrorEvent`](crate::reporting::ErrorEvent) must still resolve when the
//! reporter gets round to reading it. The cap is a disk-space guard, not a
//! promise of an exact file count.
//!
//! Persistence is best-effort by construction. Every failure path logs and
//! returns `None`: a capsule that cannot be written must never turn a 500 into
//! a worse 500.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
    )
)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, TimeDelta, Utc};

use crate::capsule::capture::CaptureScope;
use crate::capsule::redact::RedactedValues;
use crate::capsule::schema::{
    AppInfo, CAPSULE_FORMAT_VERSION, Capsule, CapsuleError, CapsuleOutcome,
};

/// How long a capsule is protected from retention pruning after it was
/// written.
///
/// A capsule's path travels to reporters on
/// [`ErrorEvent::capsule`](crate::reporting::ErrorEvent::capsule), and a
/// reporter may be an HTTP call that takes seconds to get round to reading it.
/// Deleting a capsule that recent to honour a count cap would turn the link
/// into a dangling path — the one thing the ordering guarantee exists to
/// prevent.
const PRUNE_GRACE: TimeDelta = TimeDelta::minutes(1);

/// Capsules whose paths a reporter chain is currently reading.
///
/// The grace window covers the common case, but a slow reporter can outlast
/// it; `ErrorEvent::capsule` promises a readable path for as long as the
/// reporters run, so the reporting task pins the file here for that whole
/// span and [`prune`] leaves pinned paths alone. Reference-counted, because
/// overlapping failures can pin the same directory's files independently.
static PINNED_FOR_REPORTING: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Keeps a capsule file unprunable while a reporter chain holds it.
#[derive(Debug)]
pub(crate) struct ReportingPin(PathBuf);

impl Drop for ReportingPin {
    fn drop(&mut self) {
        if let Ok(mut pinned) = PINNED_FOR_REPORTING.lock()
            && let Some(count) = pinned.get_mut(&self.0)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                pinned.remove(&self.0);
            }
        }
    }
}

/// Pin `path` against pruning until the returned guard drops.
pub(crate) fn pin_for_reporting(path: &Path) -> ReportingPin {
    if let Ok(mut pinned) = PINNED_FOR_REPORTING.lock() {
        *pinned.entry(path.to_path_buf()).or_insert(0) += 1;
    }
    ReportingPin(path.to_path_buf())
}

/// Whether a reporter chain is still reading this capsule.
fn is_pinned_for_reporting(path: &Path) -> bool {
    PINNED_FOR_REPORTING
        .lock()
        .is_ok_and(|pinned| pinned.contains_key(path))
}

/// Where a persisted capsule ended up.
///
/// Carried on [`ErrorEvent::capsule`](crate::reporting::ErrorEvent::capsule) so
/// a reporter can attach the path (or the id) to whatever it ships upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleRef {
    /// Capsule id — the request id when one was available.
    pub id: String,
    /// Absolute or project-relative path to the written capsule.
    pub path: PathBuf,
}

/// Resolve the capsule directory from the configured path.
#[must_use]
pub fn capsule_dir(dir: &str) -> PathBuf {
    PathBuf::from(dir)
}

/// Write the capsule for a finished request, returning where it landed.
///
/// Returns `None` when the capsule could not be written; the reason is logged
/// at `error` level and never propagated to the request.
#[must_use]
pub fn persist(scope: &CaptureScope, outcome: CapsuleOutcome) -> Option<CapsuleRef> {
    let capsule = assemble(scope, outcome)?;
    let settings = scope.settings();
    let dir = capsule_dir(&settings.dir);

    let json = match serde_json::to_vec_pretty(&capsule) {
        Ok(json) => json,
        Err(error) => {
            tracing::error!(%error, "failure capsule could not be serialized; dropping it");
            return None;
        }
    };

    let path = dir.join(file_name(&capsule));
    // Make room *first*, so the capsule about to be written is not a candidate
    // for the deletion that makes room for it, and a zero cap cannot mean
    // "record the failure, then immediately throw it away".
    prune(
        &dir,
        retained_before_write(settings.max_capsules),
        Utc::now(),
    );
    if let Err(error) = write_atomically(&dir, &path, &json) {
        tracing::error!(
            %error,
            path = %path.display(),
            "failure capsule could not be written; the failure itself is still reported"
        );
        return None;
    }

    Some(CapsuleRef {
        id: capsule.id,
        path,
    })
}

/// Turn a finished scope into the capsule document.
///
/// Returns `None` when the capture layer never recorded a request for this
/// scope — there is nothing replayable to write.
fn assemble(scope: &CaptureScope, outcome: CapsuleOutcome) -> Option<Capsule> {
    let raw = scope.raw_request()?;
    // The head was snapshotted when the request arrived; the body was copied
    // as the handler read it, so it is only final now. A body the handler
    // never finished reading is kept, with a note saying so.
    let raw_body = scope.captured_body();
    if let Some(note) = scope.body_note() {
        scope.note(note);
    }
    let (mut request, redacted, body_notes) =
        crate::capsule::redact::redact_request(raw, &raw_body, scope.filter());
    if let Some(identity) = scope.client_identity() {
        request.client_addr = identity.addr;
        request.client_host.clone_from(&identity.host);
        request.client_scheme.clone_from(&identity.scheme);
    }
    for note in body_notes {
        scope.note(note);
    }

    let mut db = scope.db_snapshot();
    if let Some(db) = db.as_mut() {
        for tape in &mut db.connections {
            for exchange in tape
                .prologue
                .iter_mut()
                .chain(tape.statements.iter_mut())
                .chain(tape.catalog.iter_mut())
                .chain(tape.exchanges.iter_mut())
            {
                crate::capsule::redact::mask_binds(&mut exchange.binds, &redacted);
                // A backend error message is free-form text that quotes the
                // statement's own values back: a unique-violation `DETAIL`
                // names the conflicting key, a check-constraint failure names
                // the row. That is the one place a masked bind reappears in
                // the clear, so it is scrubbed like the outcome. (The raw
                // response frames are left byte-verbatim — replay writes them
                // back to a real driver, which would reject a rewritten
                // `ErrorResponse` — and the capsule documentation says so.)
                if let Some(error) = exchange.error.as_mut() {
                    *error = crate::capsule::redact::mask_echoes(error, &redacted);
                }
            }
        }
    }

    let settings = scope.settings();
    Some(Capsule {
        format_version: CAPSULE_FORMAT_VERSION,
        id: scope.id().to_owned(),
        captured_at: Utc::now(),
        autumn_version: env!("CARGO_PKG_VERSION").to_owned(),
        app: AppInfo {
            name: settings.app_name.clone(),
            profile: settings.profile.clone(),
        },
        request,
        outcome: scrub_outcome(outcome, &redacted),
        clock: scope.clock_readings(),
        db,
        truncated: scope.is_truncated(),
        notes: scope.notes(),
    })
}

/// Mask any redacted request value the failure quoted back at us.
///
/// Redaction removes a secret from the request record, but a handler that
/// fails while talking about what it was given — `could not store
/// password=hunter2`, or a panic payload carrying the submitted value — writes
/// it straight back into the capsule through the outcome. The outcome is
/// free-form text, so this is a substring replacement rather than the
/// whole-value comparison bind masking uses.
fn scrub_outcome(outcome: CapsuleOutcome, redacted: &RedactedValues) -> CapsuleOutcome {
    use crate::capsule::redact::mask_echoes;

    match outcome {
        CapsuleOutcome::Status {
            code,
            message,
            problem_type,
        } => CapsuleOutcome::Status {
            code,
            message: mask_echoes(&message, redacted),
            // A problem type is a stable URI naming a *class* of failure, not
            // request content; masking it would only break the link.
            problem_type,
        },
        CapsuleOutcome::Panic {
            status,
            payload,
            backtrace,
        } => CapsuleOutcome::Panic {
            status,
            payload: mask_echoes(&payload, redacted),
            backtrace: backtrace.map(|frames| mask_echoes(&frames, redacted)),
        },
    }
}

/// Capsule file name: sortable timestamp, a process-local sequence number to
/// break ties within the same microsecond, then the capsule id.
fn file_name(capsule: &Capsule) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stamp = capsule.captured_at.format("%Y%m%dT%H%M%S%.6f");
    let id = sanitize_id(&capsule.id);
    format!("{stamp}-{sequence:06}-{id}.json")
}

/// Reduce an id to characters that are safe in a file name.
fn sanitize_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if sanitized.is_empty() {
        "capsule".to_owned()
    } else {
        sanitized
    }
}

/// Write owner-only, through a temp file, so a reader never sees a partial
/// capsule and the contents are never group- or world-readable.
///
/// The same shape as [`acme::store`](crate::acme) uses for private keys, and
/// for the same reason — the bytes are secret:
///
/// * the directory is created (and, on unix, *re-set*) `0o700`, because
///   `create_dir_all` applies the process umask and a permissive umask would
///   leave a world-readable directory of production request data;
/// * the temp file is opened `create_new` under a random name, so the write
///   can never follow a symlink an attacker planted at a predictable path, and
///   never truncates a file it did not create;
/// * its mode is set explicitly after the open, because `OpenOptions::mode` is
///   also umask-masked.
fn write_atomically(dir: &Path, path: &Path, json: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let temp = temp_path(path);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    {
        let mut file = options.open(&temp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(json)?;
        file.sync_all()?;
    }
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

/// An unpredictable sibling path for the temp file.
///
/// `<capsule>.json.tmp` is guessable, and `create_new` on a guessable path
/// fails outright once something already sits there — a denial of capture, and
/// on a shared `tmp/` a way to point the write somewhere else. The suffix comes
/// from the same entropy the framework uses elsewhere.
fn temp_path(path: &Path) -> PathBuf {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    path.with_extension(format!("json.{nonce}.tmp"))
}

/// How many existing capsules may remain when a new one is about to be
/// written, so that the directory settles at `max_capsules` afterwards.
///
/// `max_capsules` is user input and a zero would otherwise mean "capture the
/// failure and delete it again", so it is clamped to at least one — retention
/// tunes how much history to keep, it is not an off switch for capture.
fn retained_before_write(max_capsules: usize) -> usize {
    max_capsules.max(1).saturating_sub(1)
}

/// Delete the oldest capsules beyond `keep`, sparing anything written within
/// [`PRUNE_GRACE`].
///
/// File names begin with a sortable timestamp, so lexical order is
/// chronological order and the same prefix gives each file's age without a
/// `stat` — and, more importantly, without trusting an mtime that a copy or a
/// restore may have rewritten.
///
/// Sparing a recent file means the directory can sit *above* the cap for a
/// grace window. That is the intended trade: the cap exists to stop an error
/// storm filling a disk, and overshooting it by a minute's worth of capsules
/// costs far less than breaking the path a reporter is about to follow.
fn prune(dir: &Path, keep: usize, now: DateTime<Utc>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    if names.len() <= keep {
        return;
    }
    names.sort();
    let excess = names.len().saturating_sub(keep);
    for path in names.into_iter().take(excess) {
        if written_within_grace(&path, now) || is_pinned_for_reporting(&path) {
            continue;
        }
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(
                %error,
                path = %path.display(),
                "failure capsule could not be pruned"
            );
        }
    }
}

/// Whether a capsule file's name says it was written less than
/// [`PRUNE_GRACE`] ago.
///
/// A name this writer did not produce has no readable timestamp; it is not a
/// capsule anyone is about to follow a link to, so it is left prunable rather
/// than pinned in the directory forever.
fn written_within_grace(path: &Path, now: DateTime<Utc>) -> bool {
    let Some(stamp) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split('-').next())
    else {
        return false;
    };
    let Ok(written) = chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%S%.f") else {
        return false;
    };
    now.signed_duration_since(written.and_utc()) < PRUNE_GRACE
}

/// Read a capsule back from disk.
///
/// # Errors
///
/// Returns [`CapsuleError`] when the file cannot be read, is not a capsule, or
/// was written by an incompatible format version.
pub fn load_capsule(path: &Path) -> Result<Capsule, CapsuleError> {
    let json = std::fs::read_to_string(path).map_err(CapsuleError::Io)?;
    Capsule::from_json(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pinned_capsule_survives_pruning_past_the_grace_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Names old enough that the grace window has long lapsed; only the
        // pin can save the first one.
        let pinned = dir.path().join("20200101T000000.000000-000001-aaa.json");
        let prunable = dir.path().join("20200101T000001.000000-000002-bbb.json");
        std::fs::write(&pinned, b"{}").expect("write");
        std::fs::write(&prunable, b"{}").expect("write");

        let guard = pin_for_reporting(&pinned);
        prune(dir.path(), 0, Utc::now());
        assert!(
            pinned.exists(),
            "a capsule a reporter chain still holds must not be pruned"
        );
        assert!(
            !prunable.exists(),
            "an unpinned lapsed capsule prunes as usual"
        );

        drop(guard);
        prune(dir.path(), 0, Utc::now());
        assert!(
            !pinned.exists(),
            "dropping the pin makes the capsule prunable again"
        );
    }

    #[test]
    fn capsule_dir_is_project_relative_by_default() {
        assert_eq!(
            capsule_dir("tmp/autumn-capsules"),
            PathBuf::from("tmp/autumn-capsules")
        );
    }

    #[test]
    fn retention_always_leaves_room_for_the_capsule_being_written() {
        assert_eq!(
            retained_before_write(0),
            0,
            "a zero cap still keeps the new one"
        );
        assert_eq!(retained_before_write(1), 0);
        assert_eq!(retained_before_write(50), 49);
    }

    #[test]
    fn file_age_comes_from_the_name_the_writer_stamped() {
        let now = Utc::now();
        let capsule = test_capsule();
        let fresh = PathBuf::from(file_name(&capsule));
        assert!(
            written_within_grace(&fresh, now),
            "a capsule stamped now is inside the grace window"
        );

        let old = PathBuf::from(file_name(&Capsule {
            captured_at: now - TimeDelta::hours(1),
            ..test_capsule()
        }));
        assert!(
            !written_within_grace(&old, now),
            "an hour-old capsule is prunable"
        );

        assert!(
            !written_within_grace(Path::new("not-a-capsule.json"), now),
            "a name this writer did not produce must not be pinned in the directory"
        );
    }

    fn test_capsule() -> Capsule {
        crate::capsule::schema::test_support::capsule(
            crate::capsule::schema::test_support::request("GET", "/boom"),
            CapsuleOutcome::Status {
                code: 500,
                message: "boom".to_owned(),
                problem_type: None,
            },
        )
    }

    #[tokio::test]
    async fn persist_on_the_blocking_pool_still_yields_a_capsule_ref() {
        use std::sync::Arc;

        use crate::capsule::CaptureSettings;
        use crate::capsule::capture::CaptureScope;
        use crate::capsule::redact::RawRequest;
        use crate::log::filter::ParameterFilter;

        let dir = tempfile::tempdir().expect("tempdir");
        let settings = CaptureSettings {
            dir: dir.path().to_string_lossy().into_owned(),
            ..CaptureSettings::default()
        };
        let scope = Arc::new(CaptureScope::new(
            "req-blocking".to_owned(),
            Arc::new(settings),
            Arc::new(ParameterFilter::new(&[], &[])),
        ));
        scope.set_request(RawRequest {
            method: "GET".to_owned(),
            uri: "/boom".parse().expect("uri parses"),
            version: axum::http::Version::HTTP_11,
            headers: axum::http::HeaderMap::new(),
            route: Some("/boom".to_owned()),
        });

        // `reporting::dispatch` hands persistence to `spawn_blocking`, so the
        // whole scope has to survive the trip to another thread and the write
        // has to land there — not just when it runs inline on the worker.
        let written = tokio::task::spawn_blocking(move || {
            persist(
                &scope,
                CapsuleOutcome::Status {
                    code: 500,
                    message: "boom".to_owned(),
                    problem_type: None,
                },
            )
        })
        .await
        .expect("the blocking task must join cleanly");

        let reference = written.expect("persisting on the blocking pool must still return a ref");
        assert_eq!(reference.id, "req-blocking");
        assert!(
            reference.path.exists(),
            "the capsule must be on disk by the time the join handle resolves, so a \
             reporter following the reference cannot race the writer"
        );
        assert_eq!(
            load_capsule(&reference.path).expect("loads").request.uri,
            "/boom"
        );
    }

    /// The recorded backend error is free-form text that quotes the
    /// statement's own values back — a unique-violation `DETAIL` names the
    /// conflicting key. Bind masking blanks the parameter; without this the
    /// same bytes travelled on in the error beside it.
    #[tokio::test]
    async fn a_backend_error_quoting_a_masked_value_is_scrubbed() {
        use std::sync::Arc;

        use crate::capsule::CaptureSettings;
        use crate::capsule::redact::RawRequest;
        use crate::capsule::schema::{BindValue, ConnectionTape, Exchange, ExchangeProtocol};
        use crate::log::filter::ParameterFilter;

        let dir = tempfile::tempdir().expect("tempdir");
        let scope = Arc::new(CaptureScope::new(
            "req-error".to_owned(),
            Arc::new(CaptureSettings {
                dir: dir.path().to_string_lossy().into_owned(),
                ..CaptureSettings::default()
            }),
            Arc::new(ParameterFilter::new(&["token".to_owned()], &[])),
        ));
        scope.set_request(RawRequest {
            method: "POST".to_owned(),
            uri: "/tokens?token=sekrit-token-value"
                .parse()
                .expect("uri parses"),
            version: axum::http::Version::HTTP_11,
            headers: axum::http::HeaderMap::new(),
            route: Some("/tokens".to_owned()),
        });
        scope.with_db(|db| {
            *db.tape_mut(1) = ConnectionTape {
                id: 1,
                exchanges: vec![Exchange {
                    protocol: ExchangeProtocol::Extended,
                    sql: "INSERT INTO tokens (value) VALUES ($1)".to_owned(),
                    binds: vec![BindValue::Value(b"sekrit-token-value".to_vec())],
                    response: Vec::new(),
                    row_count: 0,
                    error: Some(
                        "23505: duplicate key value violates unique constraint \"tokens_value_key\" \
                         DETAIL: Key (value)=(sekrit-token-value) already exists."
                            .to_owned(),
                    ),
                }],
                ..ConnectionTape::default()
            };
        });

        let reference = persist(
            &scope,
            CapsuleOutcome::Status {
                code: 500,
                message: "insert failed".to_owned(),
                problem_type: None,
            },
        )
        .expect("the capsule is written");
        let written = std::fs::read_to_string(&reference.path).expect("capsule readable");

        assert!(
            !written.contains("sekrit-token-value"),
            "a masked request value must not survive in the recorded backend error: {written}"
        );
        let capsule = load_capsule(&reference.path).expect("capsule loads");
        let error = capsule
            .db
            .as_ref()
            .and_then(|db| db.connections.first())
            .and_then(|tape| tape.exchanges.first())
            .and_then(|exchange| exchange.error.clone())
            .expect("the exchange kept its error");
        assert!(
            error.contains("duplicate key") && error.contains("[FILTERED]"),
            "the error must stay readable with the value masked, got {error}"
        );
    }

    /// Capsules are production request data: the directory and the file are
    /// owner-only, and the temp file is created fresh under an unpredictable
    /// name so the write cannot be redirected through a planted symlink.
    #[cfg(unix)]
    #[tokio::test]
    async fn capsules_are_written_owner_only_into_an_owner_only_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::sync::Arc;

        use crate::capsule::CaptureSettings;
        use crate::capsule::redact::RawRequest;
        use crate::log::filter::ParameterFilter;

        let root = tempfile::tempdir().expect("tempdir");
        // A directory that does not exist yet, so `write_atomically` creates it
        // under whatever umask this process happens to have.
        let dir = root.path().join("capsules");
        let scope = Arc::new(CaptureScope::new(
            "req-perms".to_owned(),
            Arc::new(CaptureSettings {
                dir: dir.to_string_lossy().into_owned(),
                ..CaptureSettings::default()
            }),
            Arc::new(ParameterFilter::new(&[], &[])),
        ));
        scope.set_request(RawRequest {
            method: "GET".to_owned(),
            uri: "/boom".parse().expect("uri parses"),
            version: axum::http::Version::HTTP_11,
            headers: axum::http::HeaderMap::new(),
            route: None,
        });

        let reference = persist(
            &scope,
            CapsuleOutcome::Status {
                code: 500,
                message: "boom".to_owned(),
                problem_type: None,
            },
        )
        .expect("the capsule is written");

        let file_mode = std::fs::metadata(&reference.path)
            .expect("capsule metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "a capsule must be readable only by its owner"
        );
        let dir_mode = std::fs::metadata(&dir)
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "the capsule directory must not be listable by anyone else"
        );
        assert!(
            !dir.join(format!(
                "{}.tmp",
                reference
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
            ))
            .exists(),
            "the temp file must not be left behind"
        );
    }

    #[test]
    fn a_temp_path_is_unpredictable_and_ends_in_tmp() {
        let path = Path::new("tmp/capsules/20250101T000000-000000-req.json");
        let first = temp_path(path);
        let second = temp_path(path);
        assert_ne!(
            first, second,
            "a predictable temp path can be pre-created or symlinked by anyone \
             who can write the directory"
        );
        for candidate in [&first, &second] {
            assert!(
                candidate.to_string_lossy().ends_with(".tmp"),
                "the temp file must not look like a capsule to the pruner: {candidate:?}"
            );
        }
    }

    #[test]
    fn load_capsule_rejects_a_missing_file() {
        let error = load_capsule(Path::new("does/not/exist.json"))
            .expect_err("a missing capsule must be an error");
        assert!(matches!(error, CapsuleError::Io(_)));
    }

    #[test]
    fn load_capsule_round_trips_a_written_capsule() {
        use crate::capsule::schema::test_support;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capsule.json");
        let capsule = test_support::capsule(
            test_support::request("GET", "/boom"),
            CapsuleOutcome::Status {
                code: 500,
                message: "boom".to_owned(),
                problem_type: None,
            },
        );
        std::fs::write(
            &path,
            serde_json::to_string(&capsule).expect("capsule serializes"),
        )
        .expect("fixture writes");

        let loaded = load_capsule(&path).expect("a freshly written capsule must load back");
        assert_eq!(loaded.request.uri, "/boom");
    }
}
