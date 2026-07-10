//! Opt-in schema-version envelope for persisted job payloads (issue #1205).
//!
//! Job args and (later) Harvest workflow state are persisted as plain serde
//! JSON. The queue outlives any single deploy, so the moment a deploy changes a
//! payload struct — renames a field, changes a type — in-flight jobs
//! deserialize against code that no longer matches the stored bytes and land in
//! the dead-letter table. This module adds a *version envelope* so a job type
//! can declare a schema version and an upgrade path, letting a rolling deploy
//! drain old payloads cleanly instead of dead-lettering them.
//!
//! # The envelope
//!
//! A versioned payload is stored as:
//!
//! ```json
//! { "__autumn_schema_version": N, "args": <the real args value> }
//! ```
//!
//! **Opt-in only.** A job that declares no version (default version `1`) and no
//! upgrade hook is stored *exactly* as before — raw args, no envelope — so apps
//! that never declare versions see zero behavior change and the test recorder
//! sees no churn. A missing `__autumn_schema_version` therefore always reads as
//! version 1 (existing un-versioned rows read as v1).
//!
//! **Strict envelope detection.** To keep that promise even for apps whose raw
//! args legitimately contain a top-level `__autumn_schema_version` field (via
//! `#[serde(rename = "…")]` or hand-built JSON), a value is treated as an
//! envelope *only* when it is an object with **exactly** two keys —
//! `__autumn_schema_version` (a JSON integer `>= 1`) and `args`. Anything else
//! is passed through as raw args. One residual ambiguity remains and is
//! accepted: a raw payload whose shape is *exactly*
//! `{ "__autumn_schema_version": <int>, "args": <any> }` is indistinguishable
//! from a real envelope. That is the same fundamental limit any in-band envelope
//! scheme has, and far tighter than a "any object carrying the marker key"
//! check would be.
//!
//! # Composition with the tracked-payload envelope
//!
//! The tracked-job envelope (`job_tracking::wrap_tracked_payload`) and this
//! version envelope compose by nesting, with the **tracked envelope outermost**
//! and the **version envelope inside its `args`**:
//!
//! ```json
//! { "__autumn_tracked": { "k": "<hash>" },
//!   "args": { "__autumn_schema_version": 2, "args": <real args> } }
//! ```
//!
//! This ordering falls out naturally from *where* each wrap happens: the
//! `#[job]` macro version-wraps the args before calling `enqueue_tracked`,
//! which then adds the tracked envelope. It requires **no change** to
//! `take_tracked_payload`: the execution choke point strips the tracked
//! envelope first (leaving the version envelope), and the generated handler
//! decodes the version envelope second. Key-derivation
//! (`job_unique_key`/`job_concurrency_scope`) strips the tracked envelope, then
//! this module's [`split_version`], so a v1 job and its versioned re-encoding
//! hash identically and dedup still coalesces across a deploy.
//!
//! # Replay compatibility
//!
//! Because a v1 payload and its upgraded re-encoding decode to the same `Args`
//! struct — and un-versioned rows always read as v1 — a rolling deploy can run
//! old and new workers against the same queue: old workers drain raw/v1
//! payloads, new workers upgrade them. The upgrade chain is deterministic
//! (`v -> v+1` one step at a time), which keeps replay-based consumers (Harvest,
//! once wired) reproducible.

use serde::de::DeserializeOwned;
use serde_json::Value;

/// Reserved top-level key naming the stored schema version.
pub const VERSION_MARKER: &str = "__autumn_schema_version";

/// Key under which the real args live inside the version envelope. Chosen to
/// match the tracked-payload envelope's `args` key so the two envelopes read
/// consistently.
pub const VERSION_ARGS_KEY: &str = "args";

/// The version an un-versioned (marker-less) payload reads as.
pub const DEFAULT_VERSION: u32 = 1;

/// Boxed error type an upgrade hook returns on failure.
pub type UpgradeError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Signature of a job's payload-upgrade hook.
///
/// Called once per version step with the *from* version and the unwrapped args
/// value; it returns the args migrated one version forward. The framework
/// chains it (`v -> v+1`) until the args reach the current version. Because a
/// fn pointer cannot be generic over its error type, the hook must return
/// exactly [`UpgradeError`]; use `?` on any `E: Into<UpgradeError>` (which
/// includes `String`, `&str`, and most `std::error::Error` types) or
/// `.map_err(Into::into)`.
pub type UpgradeFn = fn(u32, Value) -> Result<Value, UpgradeError>;

/// Wrap `args` in the version envelope carrying `version`.
#[must_use]
pub fn wrap(version: u32, args: Value) -> Value {
    let mut obj = serde_json::Map::with_capacity(2);
    obj.insert(VERSION_MARKER.to_string(), Value::from(version));
    obj.insert(VERSION_ARGS_KEY.to_string(), args);
    Value::Object(obj)
}

/// Read the stored schema version, treating anything that is not an exact
/// version envelope as [`DEFAULT_VERSION`].
#[must_use]
pub fn read_version(value: &Value) -> u32 {
    as_version_envelope(value).map_or(DEFAULT_VERSION, |(version, _)| version)
}

/// The single shape predicate all read paths funnel through.
///
/// Recognizes `value` as a version envelope **only** when it is a JSON object
/// with *exactly* two keys — `__autumn_schema_version` (a JSON integer that
/// fits `u32` and is `>= 1`) and `args`. Any other shape (marker missing,
/// non-integer marker, no `args`, or extra keys) is **not** an envelope, so a
/// raw args payload that merely happens to contain a top-level
/// `__autumn_schema_version` field is passed through untouched.
///
/// The exactly-two-keys requirement is safe because version detection always
/// runs *after* the tracked envelope is stripped (see the module docs): a real
/// versioned payload is exactly `{__autumn_schema_version, args}` at this layer,
/// and [`wrap`] produces exactly those two keys.
fn as_version_envelope(value: &Value) -> Option<(u32, &Value)> {
    let obj = value.as_object()?;
    if obj.len() != 2 {
        return None;
    }
    let version = obj
        .get(VERSION_MARKER)?
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v >= DEFAULT_VERSION)?;
    let args = obj.get(VERSION_ARGS_KEY)?;
    Some((version, args))
}

/// Borrowing counterpart of [`strip_version`].
///
/// If `value` is a version envelope, returns `(version, inner_args)`; otherwise
/// `(DEFAULT_VERSION, value)` unchanged. Used by key-derivation, which only
/// needs to read fields off the inner args without consuming the payload.
#[must_use]
pub fn split_version(value: &Value) -> (u32, &Value) {
    as_version_envelope(value).unwrap_or((DEFAULT_VERSION, value))
}

/// Owning counterpart of [`split_version`].
///
/// If `value` is a version envelope, removes and returns `(version,
/// inner_args)`; otherwise `(DEFAULT_VERSION, value)` unchanged. Mirrors
/// [`split_version`]'s exact-shape detection, so the two never disagree on
/// whether a given value is an envelope.
#[must_use]
pub fn strip_version(value: Value) -> (u32, Value) {
    // Detect on a borrow first so the strict predicate lives in one place; only
    // consume `obj` once we know it is a real envelope.
    let Some((version, _)) = as_version_envelope(&value) else {
        return (DEFAULT_VERSION, value);
    };
    match value {
        Value::Object(mut obj) => {
            let inner = obj.remove(VERSION_ARGS_KEY).unwrap_or(Value::Null);
            (version, inner)
        }
        // Unreachable: `as_version_envelope` only returns `Some` for objects.
        other => (version, other),
    }
}

/// Which stage of version decoding failed. Kept internal; the public surface is
/// the [`JobPayloadVersionError`]'s accessors and [`std::fmt::Display`].
#[derive(Debug)]
enum VersionErrorKind {
    /// Stored version < expected and no upgrade hook is declared.
    NoUpgradePath,
    /// Stored version > expected: this worker is older than the producer.
    TooNew,
    /// An upgrade-hook step returned an error.
    UpgradeFailed,
    /// The args did not match the current struct after reaching the expected
    /// version (a shape mismatch).
    DecodeFailed,
}

/// A version/shape mismatch decoding a persisted job payload.
///
/// Distinguishable from a generic serde error: it names the job type, the
/// stored version, and the expected version, so a dead-lettered job is
/// self-diagnosing. Detect it in an already-stringified error with
/// [`is_payload_version_error`].
///
/// # Retry behavior (follow-up)
///
/// A version mismatch is a *deterministic* failure — retrying the same stored
/// bytes can never succeed. The job framework's execution outcome today only
/// distinguishes ordinary failures (retried until `max_attempts`) from panics
/// (dead-lettered immediately); there is no per-error "non-retryable" signal a
/// handler can raise. So a version-mismatch job currently burns its retry
/// budget with backoff before dead-lettering with this (precise) error. Routing
/// it straight to the dead-letter table would require a new `JobExecutionOutcome`
/// variant threaded through all three backends' retry decisions — deferred as a
/// follow-up rather than built here.
#[derive(Debug)]
pub struct JobPayloadVersionError {
    job_type: String,
    stored_version: u32,
    expected_version: u32,
    kind: VersionErrorKind,
    source: Option<UpgradeError>,
}

/// Stable substring present in every [`JobPayloadVersionError`] `Display`, so a
/// version/shape mismatch stays recognizable after it has been flattened to a
/// string in the dead-letter record. See [`is_payload_version_error`].
const ERROR_SENTINEL: &str = "stored payload version";

impl JobPayloadVersionError {
    /// The registered job type whose payload failed to decode.
    #[must_use]
    pub fn job_type(&self) -> &str {
        &self.job_type
    }

    /// The schema version the stored payload carried (missing marker → 1).
    #[must_use]
    pub const fn stored_version(&self) -> u32 {
        self.stored_version
    }

    /// The schema version the current code expects.
    #[must_use]
    pub const fn expected_version(&self) -> u32 {
        self.expected_version
    }

    /// The underlying upgrade/deserialization error, if any.
    #[must_use]
    pub fn source_error(&self) -> Option<&(dyn std::error::Error + Send + Sync + 'static)> {
        self.source.as_deref()
    }
}

impl std::fmt::Display for JobPayloadVersionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            job_type,
            stored_version,
            expected_version,
            kind,
            source,
        } = self;
        // Every arm contains `ERROR_SENTINEL` ("stored payload version") so the
        // error stays detectable once stringified.
        match kind {
            VersionErrorKind::NoUpgradePath => write!(
                f,
                "job \"{job_type}\": stored payload version {stored_version} is incompatible with \
                 expected version {expected_version} (no upgrade path)"
            ),
            VersionErrorKind::TooNew => write!(
                f,
                "job \"{job_type}\": stored payload version {stored_version} is newer than expected \
                 version {expected_version}; this worker cannot decode it (deploy the newer code, \
                 or roll back the producer)"
            ),
            VersionErrorKind::UpgradeFailed => {
                write!(
                    f,
                    "job \"{job_type}\": stored payload version {stored_version} could not be \
                     upgraded to expected version {expected_version}"
                )?;
                if let Some(source) = source {
                    write!(f, ": {source}")?;
                }
                Ok(())
            }
            VersionErrorKind::DecodeFailed => {
                // When the stored and expected versions match (the common
                // un-versioned case, both 1), naming "expected version 1" —
                // a version the user never declared — reads as noise, so drop
                // that clause. The `ERROR_SENTINEL` prefix is preserved either
                // way so `is_payload_version_error` still matches.
                if stored_version == expected_version {
                    write!(
                        f,
                        "job \"{job_type}\": stored payload version {stored_version} does not match \
                         the current args shape"
                    )?;
                } else {
                    write!(
                        f,
                        "job \"{job_type}\": stored payload version {stored_version} does not match \
                         the current args shape for expected version {expected_version}"
                    )?;
                }
                if let Some(source) = source {
                    write!(f, ": {source}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for JobPayloadVersionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|s| s.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// Whether an already-stringified error came from a [`JobPayloadVersionError`].
///
/// Distinguishes a version/shape mismatch dead-letter from a generic serde
/// failure once the typed error has been flattened into the dead-letter
/// record's message column.
#[must_use]
pub fn is_payload_version_error(message: &str) -> bool {
    message.contains(ERROR_SENTINEL)
}

/// Decode a stored payload into `T`, applying the declared upgrade chain.
///
/// Reads the stored version `v` (missing marker → [`DEFAULT_VERSION`]). Then:
/// - `v > expected_version` → [`VersionErrorKind::TooNew`] error;
/// - while `v < expected_version`: if no `upgrade` hook, a
///   [`VersionErrorKind::NoUpgradePath`] error; otherwise apply the hook one
///   step (`v -> v+1`);
/// - finally `serde_json::from_value::<T>`; a failure here is a shape mismatch
///   reported as [`VersionErrorKind::DecodeFailed`] (never a bare serde error).
///
/// For the common un-versioned case (`expected_version == 1`, no marker, no
/// upgrade) this reduces to a plain `from_value` — identical to the pre-#1205
/// behavior, just with a more precise error on failure.
///
/// # Errors
///
/// Returns [`JobPayloadVersionError`] on any version mismatch, failed upgrade
/// step, or shape mismatch, always naming the job type and both versions.
pub fn decode_versioned<T: DeserializeOwned>(
    job_type: &str,
    expected_version: u32,
    upgrade: Option<UpgradeFn>,
    stored: Value,
) -> Result<T, JobPayloadVersionError> {
    let (stored_version, mut args) = strip_version(stored);

    if stored_version > expected_version {
        return Err(JobPayloadVersionError {
            job_type: job_type.to_string(),
            stored_version,
            expected_version,
            kind: VersionErrorKind::TooNew,
            source: None,
        });
    }

    let mut version = stored_version;
    while version < expected_version {
        let Some(upgrade) = upgrade else {
            return Err(JobPayloadVersionError {
                job_type: job_type.to_string(),
                stored_version,
                expected_version,
                kind: VersionErrorKind::NoUpgradePath,
                source: None,
            });
        };
        args = upgrade(version, args).map_err(|source| JobPayloadVersionError {
            job_type: job_type.to_string(),
            stored_version,
            expected_version,
            kind: VersionErrorKind::UpgradeFailed,
            source: Some(source),
        })?;
        version += 1;
    }

    serde_json::from_value(args).map_err(|source| JobPayloadVersionError {
        job_type: job_type.to_string(),
        stored_version,
        expected_version,
        kind: VersionErrorKind::DecodeFailed,
        source: Some(Box::new(source)),
    })
}

/// CI-time golden-fixture guard: round-trip a *stored* sample payload through
/// the version-aware decode into the current `T` and assert success.
///
/// Load a golden JSON fixture (a payload as it is actually stored — versioned
/// or raw) and call this with the job's current `expected_version` and upgrade
/// hook. A breaking change to `T` (that the upgrade chain does not cover) then
/// fails in CI rather than in the queue. See the issue's falsifiable case: with
/// the upgrade hook a v1 fixture decodes; remove the hook and this guard fails.
///
/// # Panics
///
/// Panics with the precise [`JobPayloadVersionError`] if `stored_json` does not
/// decode into `T` at `expected_version`, or if `stored_json` is not valid JSON.
pub fn assert_golden_payload<T: DeserializeOwned>(
    job_type: &str,
    expected_version: u32,
    upgrade: Option<UpgradeFn>,
    stored_json: &str,
) {
    let stored: Value = serde_json::from_str(stored_json)
        .unwrap_or_else(|e| panic!("golden fixture for job \"{job_type}\" is not valid JSON: {e}"));
    if let Err(error) = decode_versioned::<T>(job_type, expected_version, upgrade, stored) {
        panic!("golden payload guard failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[test]
    fn wrap_then_strip_is_identity_on_args() {
        let args = json!({ "a": 1, "b": [2, 3] });
        let (version, inner) = strip_version(wrap(4, args.clone()));
        assert_eq!(version, 4);
        assert_eq!(inner, args);
    }

    #[test]
    fn missing_marker_reads_as_default_version() {
        let raw = json!({ "a": 1 });
        assert_eq!(read_version(&raw), DEFAULT_VERSION);
        let (version, inner) = split_version(&raw);
        assert_eq!(version, DEFAULT_VERSION);
        assert_eq!(inner, &raw);
        // Non-objects, too.
        assert_eq!(read_version(&Value::Null), DEFAULT_VERSION);
        assert_eq!(strip_version(Value::Null), (DEFAULT_VERSION, Value::Null));
    }

    #[test]
    fn marker_without_args_is_not_an_envelope() {
        // Marker present but no `args` field (only one key): NOT an envelope, so
        // the whole object passes through as raw args at version 1. (Codex #1205
        // collision fix: strict exact-shape detection.)
        let raw = json!({ VERSION_MARKER: 5, "other": 1 });
        assert_eq!(read_version(&raw), DEFAULT_VERSION);
        let (sv, si) = split_version(&raw);
        assert_eq!(sv, DEFAULT_VERSION);
        assert_eq!(si, &raw);
        let (tv, ti) = strip_version(raw.clone());
        assert_eq!(tv, DEFAULT_VERSION);
        assert_eq!(ti, raw);
    }

    #[test]
    fn extra_keys_alongside_marker_and_args_is_not_an_envelope() {
        // Three keys: a real envelope has exactly two, so this is raw args.
        let raw = json!({ VERSION_MARKER: 5, "args": { "x": 1 }, "extra": 2 });
        assert_eq!(read_version(&raw), DEFAULT_VERSION);
        assert_eq!(split_version(&raw), (DEFAULT_VERSION, &raw));
        assert_eq!(strip_version(raw.clone()), (DEFAULT_VERSION, raw));
    }

    #[test]
    fn non_integer_marker_is_not_an_envelope() {
        // Marker present with the right sibling key but a string value: raw.
        let raw = json!({ VERSION_MARKER: "v3", "args": { "x": 1 } });
        assert_eq!(read_version(&raw), DEFAULT_VERSION);
        assert_eq!(split_version(&raw), (DEFAULT_VERSION, &raw));
        assert_eq!(strip_version(raw.clone()), (DEFAULT_VERSION, raw));
    }

    #[test]
    fn raw_payload_with_marker_field_decodes_whole_object() {
        // A raw args struct that legitimately carries a top-level
        // `__autumn_schema_version` field must deserialize as-is, not as a
        // stripped envelope subtree — the "zero behavior change" guarantee.
        #[derive(Debug, Deserialize, PartialEq, Eq)]
        struct Raw {
            #[serde(rename = "__autumn_schema_version")]
            marker: u32,
            other: i64,
        }
        let raw = json!({ "__autumn_schema_version": 5, "other": 7 });
        let decoded: Raw = decode_versioned("j", 1, None, raw).unwrap();
        assert_eq!(
            decoded,
            Raw {
                marker: 5,
                other: 7
            }
        );
    }

    #[test]
    fn exact_two_key_envelope_is_detected() {
        // The real envelope shape still matches and round-trips.
        let env = wrap(3, json!({ "x": 1 }));
        assert_eq!(read_version(&env), 3);
        assert_eq!(split_version(&env), (3, &json!({ "x": 1 })));
        assert_eq!(strip_version(env), (3, json!({ "x": 1 })));
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct Args {
        user_id: i64,
        greeting: String,
    }

    fn upgrade(from: u32, mut value: Value) -> Result<Value, UpgradeError> {
        if from == 1 {
            value
                .as_object_mut()
                .ok_or("expected object")?
                .insert("greeting".into(), json!("hi"));
        }
        Ok(value)
    }

    #[test]
    fn decode_upgrades_missing_marker_as_v1() {
        // A raw (un-versioned) v1 payload upgrades to the current v2 struct.
        let stored = json!({ "user_id": 3 });
        let decoded: Args = decode_versioned("welcome", 2, Some(upgrade), stored).unwrap();
        assert_eq!(
            decoded,
            Args {
                user_id: 3,
                greeting: "hi".into()
            }
        );
    }

    #[test]
    fn decode_at_current_version_needs_no_upgrade() {
        let stored = wrap(2, json!({ "user_id": 3, "greeting": "yo" }));
        let decoded: Args = decode_versioned("welcome", 2, None, stored).unwrap();
        assert_eq!(
            decoded,
            Args {
                user_id: 3,
                greeting: "yo".into()
            }
        );
    }

    #[test]
    fn no_upgrade_path_error_names_job_and_versions() {
        let stored = wrap(1, json!({ "user_id": 3 }));
        let err = decode_versioned::<Args>("send_welcome", 3, None, stored).unwrap_err();
        assert_eq!(err.job_type(), "send_welcome");
        assert_eq!(err.stored_version(), 1);
        assert_eq!(err.expected_version(), 3);
        let msg = err.to_string();
        assert!(is_payload_version_error(&msg), "detectable: {msg}");
        assert!(msg.contains("no upgrade path"), "{msg}");
    }

    #[test]
    fn too_new_error_when_stored_exceeds_expected() {
        let stored = wrap(5, json!({ "user_id": 3, "greeting": "hi" }));
        let err = decode_versioned::<Args>("welcome", 2, Some(upgrade), stored).unwrap_err();
        assert_eq!(err.stored_version(), 5);
        assert_eq!(err.expected_version(), 2);
        assert!(err.to_string().contains("newer than expected"));
        assert!(is_payload_version_error(&err.to_string()));
    }

    #[test]
    fn upgrade_failure_surfaces_source_and_stays_detectable() {
        fn bad_upgrade(_from: u32, _v: Value) -> Result<Value, UpgradeError> {
            Err("boom".into())
        }
        let stored = wrap(1, json!({ "user_id": 3 }));
        let err = decode_versioned::<Args>("welcome", 2, Some(bad_upgrade), stored).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("boom"), "{msg}");
        assert!(is_payload_version_error(&msg));
    }

    #[test]
    fn shape_mismatch_at_current_version_is_a_version_error_not_bare_serde() {
        // Right version, wrong shape (missing `greeting`, no upgrade to add it).
        let stored = wrap(2, json!({ "user_id": 3 }));
        let err = decode_versioned::<Args>("welcome", 2, None, stored).unwrap_err();
        assert!(
            err.to_string()
                .contains("does not match the current args shape")
        );
        assert!(is_payload_version_error(&err.to_string()));
    }

    #[test]
    fn is_payload_version_error_rejects_generic_messages() {
        assert!(!is_payload_version_error(
            "job args deserialization failed: missing field"
        ));
    }
}
