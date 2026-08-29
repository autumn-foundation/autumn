//! Writing capsules to disk, and reading them back.
//!
//! Capsules land in a plain directory of JSON files (`tmp/autumn-capsules` by
//! default, project-relative like the maintenance flag file). Because the
//! contents are real production request data, the writer is deliberately
//! conservative: owner-only permissions on unix, a temp-then-rename so a
//! reader never sees a half-written file, and an oldest-first prune so an
//! error storm cannot fill a disk.
//!
//! Retention errs towards keeping what a reporter is still reading, without
//! letting that instinct unbound the directory. `max_capsules` is clamped to
//! at least one, and pruning runs *before* the new capsule is written, so the
//! capsule this request just produced can never be the one deleted to make
//! room for it. A capsule handed to the reporting pipeline is **pinned before
//! its file becomes visible** ([`persist_pinned`]) and stays pinned until the
//! whole reporter chain finishes, so the path on an
//! [`ErrorEvent`](crate::reporting::ErrorEvent) always resolves — with no
//! window between write and pin for a concurrent prune to slip through.
//! A bounded [`PRUNE_GRACE`] additionally spares a *limited number* of the
//! newest over-cap files (cross-process writers have no way to see this
//! process's pins); the bound is what keeps a sub-minute failure storm from
//! accumulating an unbounded directory. The cap is a disk-space guard, not a
//! promise of an exact file count: the directory can briefly hold up to
//! roughly twice `max_capsules`, plus whatever reporters still hold pinned.
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
        clippy::string_slice,
        clippy::arithmetic_side_effects,
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
use crate::log::filter::ParameterFilter;

/// How long a capsule is protected from retention pruning after it was
/// written — for a *bounded number* of files (see [`grace_allowance`]).
///
/// In-process readers are protected exactly, by pinning: [`persist_pinned`]
/// registers the file before it becomes visible, so this window is not what
/// keeps a reporter's path alive. It exists for writers this process cannot
/// see — a second app process sharing the capsule directory pins in its own
/// memory, and pruning a file that process wrote a second ago would break the
/// path its reporters are about to follow.
const PRUNE_GRACE: TimeDelta = TimeDelta::minutes(1);

/// Why a capsule is refused after the parameter filter removed a resolved
/// client-identity field the request actually had.
const IDENTITY_FILTERED_NOTE: &str = "the resolved client identity was suppressed because a header it derives from is in \
     `[log] filter_parameters`: this capsule cannot reproduce the identity the handler saw";

/// How many over-cap files [`PRUNE_GRACE`] may spare in one prune pass.
///
/// Ungated, a failure storm faster than the grace window would make every
/// over-cap file "too recent to delete" and the directory unbounded — the
/// exact disk-fill the cap exists to stop. Capping the sparing at one cap's
/// worth bounds the directory at roughly twice `max_capsules` (plus pinned
/// files), which keeps the cross-process courtesy without giving up the
/// guard.
fn grace_allowance(keep: usize) -> usize {
    keep.max(1)
}

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
        let count = pinned.entry(path.to_path_buf()).or_insert(0);
        *count = count.saturating_add(1);
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
///
/// The file is briefly pinned during the write and released before this
/// returns; a caller that will *read the file back later* — the reporting
/// pipeline — must use [`persist_pinned`] instead, so there is no window in
/// which a concurrent failure's prune can delete the file first.
#[must_use]
pub fn persist(scope: &CaptureScope, outcome: CapsuleOutcome) -> Option<CapsuleRef> {
    persist_pinned(scope, outcome).map(|(reference, _pin)| reference)
}

/// Serializes the prune-then-write retention transaction: during a failure
/// burst, several blocking-pool persistence tasks would otherwise all prune
/// against the same under-cap directory listing and then all write,
/// overshooting `max_capsules` by the burst's width with nothing to clean it
/// up until the next failure. The critical section is short (a directory scan
/// and one file write), and blocking is fine — persistence already runs on
/// the blocking pool.
static RETENTION: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// [`persist`], holding the written file pinned against pruning.
///
/// The pin is taken **before** the file becomes visible in the directory, so
/// from the first instant a concurrent [`prune`] can observe the file it is
/// already unprunable — the write-to-pin race a caller-side pin would leave
/// open. Dropping the returned [`ReportingPin`] makes the file ordinary
/// retention fodder again. The whole prune-then-write pair runs under
/// [`RETENTION`], so concurrent persists cannot overshoot the cap.
#[must_use]
pub(crate) fn persist_pinned(
    scope: &CaptureScope,
    outcome: CapsuleOutcome,
) -> Option<(CapsuleRef, ReportingPin)> {
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
    let _retention = RETENTION.lock();
    // Make room *first*, so the capsule about to be written is not a candidate
    // for the deletion that makes room for it, and a zero cap cannot mean
    // "record the failure, then immediately throw it away".
    prune(
        &dir,
        retained_before_write(settings.max_capsules),
        Utc::now(),
    );
    let pin = pin_for_reporting(&path);
    if let Err(error) = write_atomically(&dir, &path, &json) {
        tracing::error!(
            %error,
            path = %path.display(),
            "failure capsule could not be written; the failure itself is still reported"
        );
        return None;
    }

    Some((
        CapsuleRef {
            id: capsule.id,
            path,
        },
        pin,
    ))
}

/// Turn a finished scope into the capsule document.
///
/// Returns `None` when the capture layer never recorded a request for this
/// scope — there is nothing replayable to write.
fn assemble(scope: &CaptureScope, outcome: CapsuleOutcome) -> Option<Capsule> {
    let raw = scope.raw_request()?;
    // The head was snapshotted when the request arrived; the body was copied
    // as the handler read it, so it is only final now.
    let raw_body = scope.captured_body();
    if let Some(note) = scope.body_note() {
        // A body the handler did not read to its end — a prefix, or nothing at
        // all — is *not* the body the failing request carried. Replaying it
        // would drive the handler with shorter input than production had, and
        // code that reads the remainder would hit EOF and answer differently:
        // a `mismatch`, which the guide tells operators to read as "the bug is
        // gone". Mark the capsule incomplete so replay refuses it instead,
        // through the same path a skipped body already takes. The note says
        // which case this was.
        scope.note(note);
        scope.mark_truncated();
    }
    // The scheme the URI already carries, captured before the request record
    // is built: a resolved scheme equal to it did not come from a header.
    let uri_scheme = raw.uri.scheme_str().map(ToOwned::to_owned);
    let (mut request, mut redacted, body_notes) =
        crate::capsule::redact::redact_request(raw, &raw_body, scope.filter());
    // The effect tape is masked through the *same* filter, and into the same
    // echo set, before anything else is scrubbed: a secret a handler sent to a
    // third party, or wrote into a cache value, must be gone from the outcome
    // and the SQL binds too. Recording raw and masking here (rather than at
    // each seam) keeps redaction's cost off requests that never fail.
    let mut effects = scope.effects_snapshot();
    // A job capsule's entry point is a payload like any other: it is the job's
    // *arguments*, and they carry tokens and PII exactly the way a request body
    // does. Redacted alongside the effect tape, through the same filter and
    // into the same echo set.
    let mut job = scope.job_entry();
    let mut effect_keys = std::collections::BTreeSet::new();
    crate::capsule::redact::redact_effects(
        &mut effects,
        job.as_mut(),
        scope.filter(),
        &mut redacted,
        &mut effect_keys,
    );
    request.redacted_keys.extend(effect_keys);
    request.redacted_keys.sort_unstable();
    request.redacted_keys.dedup();
    request.peer_addr = scope.peer_addr();
    if let Some(identity) = scope.client_identity() {
        // The resolved identity is *derived from* headers, so it must obey the
        // same filter those headers do. An operator who adds
        // `x-forwarded-host` to `[log] filter_parameters` sees it masked in
        // `headers` — and would otherwise find the very same
        // `private-tenant.example` sitting in cleartext in `client_host`,
        // which defeats the filter through a side door rather than honoring
        // it. Each field is dropped when *any* header it could have been
        // resolved from is filtered: which one the resolver actually used
        // depends on the trust configuration and the request, and recording
        // the value only when it happened to come from an unfiltered header
        // would leak on exactly the requests where the filtered one won.
        //
        // The lists are exactly what `ProxyResolver` reads — `x-forwarded-*`,
        // `x-real-ip`, `Host` — and nothing else. `Forwarded` is *not* among
        // them, and naming it here suppressed all three fields, and refused
        // the capsule, over a header the resolver never looks at.
        //
        // A value that did not come from a header at all is exempt, and not as
        // a courtesy: the address then came from the peer socket and the
        // scheme from the request URI, both of which this capsule records
        // unfiltered a few lines either side. Suppressing a copy of a value
        // that is already written down in the clear protects nothing and
        // costs a refusal.
        let filter = scope.filter();
        let mut suppressed = false;
        let addr_from_peer =
            identity.addr.is_some() && identity.addr == scope.peer_addr().map(|peer| peer.ip());
        if !addr_from_peer && identity_source_is_filtered(filter, &["x-forwarded-for", "x-real-ip"])
        {
            suppressed |= identity.addr.is_some();
        } else {
            request.client_addr = identity.addr;
        }
        if identity_source_is_filtered(filter, &["x-forwarded-host", "host"]) {
            suppressed |= identity.host.is_some();
        } else {
            request.client_host.clone_from(&identity.host);
        }
        let scheme_from_uri = identity.scheme.is_some() && identity.scheme == uri_scheme;
        if !scheme_from_uri && identity_source_is_filtered(filter, &["x-forwarded-proto"]) {
            suppressed |= identity.scheme.is_some();
        } else {
            request.client_scheme.clone_from(&identity.scheme);
        }
        // Suppression protects the operator's filter, but it does not leave a
        // *faithful* capsule: replay pre-inserts the recorded identity whole
        // whenever any field survives, and `TrustedProxiesLayer` honors it
        // rather than re-resolving — so a handler reading `ClientHost` would
        // meet a `None` production never gave it and answer differently. That
        // is a `mismatch` the guide tells operators to read as "the bug is
        // gone". Refusing is the honest outcome, and it fires only when a
        // filtered source actually supplied a value: filtering a header the
        // request never carried costs nothing.
        if suppressed {
            scope.note(IDENTITY_FILTERED_NOTE);
            scope.mark_truncated();
        }
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
            // The build that *recorded* is by definition the build that ran,
            // so this is read here rather than configured: replay compiles
            // the same way and `cfg(debug_assertions)` code paths line up.
            debug_assertions: Some(cfg!(debug_assertions)),
        },
        request,
        outcome: scrub_outcome(outcome, &redacted),
        clock: scope.clock_readings(),
        clock_monotonic_us: scope
            .monotonic_readings()
            .into_iter()
            .map(|offset| u64::try_from(offset.as_micros()).unwrap_or(u64::MAX))
            .collect(),
        db,
        db_roles: settings.db_roles.clone(),
        truncated: scope.is_truncated(),
        notes: scope.notes(),
        effects,
        job,
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

/// Delete the oldest capsules beyond `keep`, sparing pinned files always and
/// files written within [`PRUNE_GRACE`] up to [`grace_allowance`].
///
/// Only names produced by [`file_name`] are candidates at all
/// ([`capsule_stamp`]): the capture directory is user configuration and may
/// hold files this module did not write — an application's `state.json`
/// sitting next to the capsules is not retention fodder.
///
/// File names begin with a sortable timestamp, so lexical order is
/// chronological order and the same prefix gives each file's age without a
/// `stat` — and, more importantly, without trusting an mtime that a copy or a
/// restore may have rewritten.
///
/// Sparing means the directory can sit *above* the cap — by the bounded grace
/// allowance, plus whatever reporters still hold pinned. The grace sparing is
/// deliberately newest-first: when a storm forces a choice among recent
/// files, the ones most likely to still be in a reporter's hands (in another
/// process, whose pins this one cannot see) are the newest.
fn prune(dir: &Path, keep: usize, now: DateTime<Utc>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| capsule_stamp(path).is_some())
        .collect();
    if names.len() <= keep {
        return;
    }
    names.sort();
    let excess = names.len().saturating_sub(keep);
    let mut allowance = grace_allowance(keep);
    // Walk the excess newest-first so the bounded allowance goes to the most
    // recently written files; everything past the allowance is deleted even
    // when it is within the grace window.
    for path in names.into_iter().take(excess).rev() {
        if is_pinned_for_reporting(&path) {
            continue;
        }
        if allowance > 0 && written_within_grace(&path, now) {
            allowance = allowance.saturating_sub(1);
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
fn written_within_grace(path: &Path, now: DateTime<Utc>) -> bool {
    capsule_stamp(path)
        .is_some_and(|written| now.signed_duration_since(written.and_utc()) < PRUNE_GRACE)
}

/// Whether any header a resolved-identity field could come from is filtered.
///
/// The resolver's choice among these depends on the trust configuration and on
/// what the request actually carried, and none of that is knowable here — so
/// *any* filtered source suppresses the derived field. Recording it whenever
/// the value happened to arrive through an unfiltered header would leak on
/// precisely the requests where the filtered one won, which is the case the
/// operator was guarding against.
fn identity_source_is_filtered(filter: &ParameterFilter, sources: &[&str]) -> bool {
    sources.iter().any(|source| filter.matches_key(source))
}

/// The timestamp a [`file_name`]-shaped name carries, or `None` for any other
/// file.
///
/// This is what makes a file *this module's to delete*: the whole shape is
/// checked — `<timestamp>-<6-digit sequence>-<id>.json` — not just a `.json`
/// extension, because `[failure_capture] dir` points wherever the user says
/// and pruning must never eat a file some other writer put there.
fn capsule_stamp(path: &Path) -> Option<chrono::NaiveDateTime> {
    let name = path.file_name()?.to_str()?.strip_suffix(".json")?;
    let (stamp, rest) = name.split_once('-')?;
    let (sequence, id) = rest.split_once('-')?;
    if sequence.len() != 6 || !sequence.bytes().all(|b| b.is_ascii_digit()) || id.is_empty() {
        return None;
    }
    chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%S%.f").ok()
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

    /// `[failure_capture] dir` is user configuration: an application is free
    /// to keep its own files next to the capsules, and retention must never
    /// delete what it did not write — even at `keep = 0`, and even for names
    /// that *almost* look like capsules.
    #[test]
    fn pruning_never_deletes_files_the_capsule_writer_did_not_create() {
        let dir = tempfile::tempdir().expect("tempdir");
        let foreign = [
            "state.json",
            "aaa.json",
            // Timestamp but no sequence/id: not a name `file_name` produces.
            "20200101T000000.000000-boom.json",
            // Sequence of the wrong width.
            "20200101T000000.000000-01-boom.json",
            // No parseable timestamp in front.
            "notastamp-000001-boom.json",
        ];
        for name in foreign {
            std::fs::write(dir.path().join(name), b"{}").expect("write");
        }
        let capsule = dir.path().join("20200101T000000.000000-000001-aaa.json");
        std::fs::write(&capsule, b"{}").expect("write");

        prune(dir.path(), 0, Utc::now());

        assert!(!capsule.exists(), "the real capsule is over the cap");
        for name in foreign {
            assert!(
                dir.path().join(name).exists(),
                "{name} was not written by capsule persistence and must survive"
            );
        }
    }

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

    /// A failure storm faster than the grace window must not make every
    /// over-cap file "too recent to delete": the grace spares a bounded
    /// number of the newest files, and the rest prune regardless of age.
    #[test]
    fn a_sub_minute_storm_cannot_grow_the_directory_unbounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let now = Utc::now();
        // Twenty capsules all stamped within the last few seconds — the
        // storm case where the old unbounded grace spared every one.
        let stamp = (now - TimeDelta::seconds(5)).format("%Y%m%dT%H%M%S%.6f");
        for sequence in 0..20 {
            let name = format!("{stamp}-{sequence:06}-storm.json");
            std::fs::write(dir.path().join(name), b"{}").expect("write");
        }

        let keep = 2;
        prune(dir.path(), keep, now);

        let survivors = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .count();
        assert_eq!(
            survivors,
            keep + grace_allowance(keep),
            "the directory must settle at the cap plus the bounded grace \
             allowance, not at whatever the storm produced"
        );
    }

    /// The resolved identity is derived from headers, so filtering one of
    /// those headers must suppress it. Masking `x-forwarded-host` in `headers`
    /// while writing the same value to `client_host` honors the filter in
    /// letter and defeats it in fact.
    #[test]
    fn a_filtered_identity_header_suppresses_the_derived_field() {
        use std::sync::Arc;

        use crate::capsule::CaptureSettings;
        use crate::capsule::CapturedClientIdentity;
        use crate::capsule::capture::CaptureScope;
        use crate::capsule::redact::RawRequest;
        use crate::log::filter::ParameterFilter;

        let build = |filtered: &[String]| {
            let scope = CaptureScope::new(
                "req-identity".to_owned(),
                Arc::new(CaptureSettings::default()),
                Arc::new(ParameterFilter::new(filtered, &[])),
            );
            scope.set_request(RawRequest {
                method: "GET".to_owned(),
                uri: "/boom".parse().expect("uri parses"),
                version: axum::http::Version::HTTP_11,
                headers: axum::http::HeaderMap::new(),
                route: None,
            });
            scope.set_client_identity(CapturedClientIdentity {
                addr: Some("203.0.113.7".parse().expect("addr parses")),
                host: Some("private-tenant.example".to_owned()),
                scheme: Some("https".to_owned()),
            });
            assemble(
                &scope,
                CapsuleOutcome::Status {
                    code: 500,
                    message: "boom".to_owned(),
                    problem_type: None,
                },
            )
            .expect("capsule assembles")
        };

        let unfiltered = build(&[]);
        assert_eq!(
            unfiltered.request.client_host.as_deref(),
            Some("private-tenant.example"),
            "with nothing filtered the identity is recorded as before"
        );

        assert!(
            !unfiltered.truncated,
            "nothing suppressed, nothing to refuse"
        );

        let filtered = build(&["x-forwarded-host".to_owned()]);
        assert_eq!(
            filtered.request.client_host, None,
            "a filtered identity header must not reappear as `client_host`"
        );
        // Replay pre-inserts the recorded identity whole whenever any field
        // survives, so a suppressed host reaches the handler as `None` rather
        // than not at all. That is not a faithful capsule, and claiming it is
        // would report a false `mismatch`.
        assert!(
            filtered.truncated,
            "a capsule that cannot reproduce the identity must be refused, not replayed"
        );
        assert_eq!(
            filtered.request.client_addr,
            Some("203.0.113.7".parse().expect("addr parses")),
            "filtering one source must not suppress fields it cannot feed"
        );
        assert_eq!(
            filtered.request.client_scheme.as_deref(),
            Some("https"),
            "nor the scheme, which `x-forwarded-host` cannot resolve"
        );

        // `Forwarded` is not a source: `ProxyResolver` never reads it, so
        // filtering it must suppress nothing and refuse nothing.
        let forwarded = build(&["forwarded".to_owned()]);
        assert_eq!(
            forwarded.request.client_host.as_deref(),
            Some("private-tenant.example")
        );
        assert_eq!(
            forwarded.request.client_addr,
            Some("203.0.113.7".parse().expect("addr parses"))
        );
        assert_eq!(forwarded.request.client_scheme.as_deref(), Some("https"));
        assert!(
            !forwarded.truncated,
            "a header the resolver never reads must not refuse the capsule"
        );

        // `X-Real-IP` is a fallback source for the address, so filtering it
        // suppresses the address too.
        let real_ip = build(&["x-real-ip".to_owned()]);
        assert_eq!(real_ip.request.client_addr, None);
        assert_eq!(
            real_ip.request.client_host.as_deref(),
            Some("private-tenant.example"),
            "and only the address — it feeds nothing else"
        );
    }

    /// Refusal is the price of suppressing a value that existed. Filtering a
    /// header the request resolved nothing from suppresses nothing, so it must
    /// cost nothing — otherwise a broad `filter_parameters` would refuse every
    /// capsule an application ever writes.
    #[test]
    fn filtering_a_source_that_supplied_nothing_does_not_refuse_the_capsule() {
        use std::sync::Arc;

        use crate::capsule::CaptureSettings;
        use crate::capsule::CapturedClientIdentity;
        use crate::capsule::capture::CaptureScope;
        use crate::capsule::redact::RawRequest;
        use crate::log::filter::ParameterFilter;

        let scope = CaptureScope::new(
            "req-identity".to_owned(),
            Arc::new(CaptureSettings::default()),
            Arc::new(ParameterFilter::new(&["x-real-ip".to_owned()], &[])),
        );
        scope.set_request(RawRequest {
            method: "GET".to_owned(),
            uri: "/boom".parse().expect("uri parses"),
            version: axum::http::Version::HTTP_11,
            headers: axum::http::HeaderMap::new(),
            route: None,
        });
        scope.set_client_identity(CapturedClientIdentity {
            addr: None,
            host: Some("private-tenant.example".to_owned()),
            scheme: None,
        });

        let capsule = assemble(
            &scope,
            CapsuleOutcome::Status {
                code: 500,
                message: "boom".to_owned(),
                problem_type: None,
            },
        )
        .expect("capsule assembles");

        assert!(
            !capsule.truncated,
            "filtering a source that supplied nothing must not refuse the capsule"
        );
        assert_eq!(
            capsule.request.client_host.as_deref(),
            Some("private-tenant.example"),
            "and the fields it does not feed are still recorded"
        );
    }

    /// The reporting pipeline's pin is taken inside the write, before the
    /// file becomes visible — so the moment another failure's prune can see
    /// the file, it is already unprunable. A caller-side pin would leave a
    /// window between `persist` returning and the pin being taken.
    #[test]
    fn persist_pinned_writes_a_file_that_is_already_pinned() {
        use std::sync::Arc;

        use crate::capsule::CaptureSettings;
        use crate::capsule::capture::CaptureScope;
        use crate::capsule::redact::RawRequest;
        use crate::log::filter::ParameterFilter;

        let dir = tempfile::tempdir().expect("tempdir");
        let scope = Arc::new(CaptureScope::new(
            "req-pinned".to_owned(),
            Arc::new(CaptureSettings {
                dir: dir.path().to_string_lossy().into_owned(),
                max_capsules: 1,
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

        let (reference, pin) = persist_pinned(
            &scope,
            CapsuleOutcome::Status {
                code: 500,
                message: "boom".to_owned(),
                problem_type: None,
            },
        )
        .expect("the capsule is written");

        assert!(
            is_pinned_for_reporting(&reference.path),
            "the file must already be pinned when persist_pinned returns"
        );
        // A concurrent failure's prune — zero retention, grace long lapsed
        // (simulated by pruning far in the future) — must not take it.
        prune(dir.path(), 0, Utc::now() + TimeDelta::hours(1));
        assert!(
            reference.path.exists(),
            "a pinned capsule survives a concurrent prune even past the grace"
        );

        drop(pin);
        assert!(
            !is_pinned_for_reporting(&reference.path),
            "dropping the pin releases the file to ordinary retention"
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
