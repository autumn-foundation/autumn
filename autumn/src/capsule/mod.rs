//! Deterministic replay capsules: record a failed request, replay it offline.
//!
//! When `[failure_capture] enabled = true`, every failing request (a caught
//! panic or a 5xx) is written to disk as a *capsule* — a single JSON file
//! holding the redacted request, the clock readings the handler took, the
//! database traffic it generated, and the outcome the client received. The
//! capsule is written *before* any [`ErrorReporter`](crate::reporting::ErrorReporter)
//! runs, so a reporter can attach
//! [`ErrorEvent::capsule`](crate::reporting::ErrorEvent::capsule) to whatever
//! it ships upstream and the file is already there when someone follows the
//! link.
//!
//! # Security
//!
//! **A capsule contains real production request data.** Headers, query
//! parameters and structured bodies are masked through the same
//! `[log] filter_parameters` list the access log uses (see
//! [`redact`]), and any SQL bind that echoes a masked value is blanked — but
//! unstructured bodies, URL paths, and database result rows are *not* scanned.
//! Capsules are written owner-only into a directory you should treat like a
//! log of production traffic. Capture is off by default.
//!
//! # Layout
//!
//! * [`schema`] — the on-disk document and its version gate.
//! * [`redact`] — masking, and the redacted-value set that feeds bind masking.
//! * [`persist`] — writing, pruning, and reading capsules back.
//! * [`capture`] — the request-scoped buffer and the Tower layer.
//! * [`clock`] — the recording and replaying clock sources.
//! * [`replay`] — the replay driver, the divergence log and the verdict.
//! * `replay_db` — the in-process stub `PostgreSQL` server replay reads from.

pub mod capture;
pub mod clock;
pub mod persist;
pub mod redact;
pub mod replay;
pub mod schema;

// DB wire submodules (PostgreSQL only; the sqlite backend has no wire capture).
// Crate-private like `wire`: these are the recorder and the replay stub, not an
// extension point, and everything a caller needs is re-exported below.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
pub(crate) mod record_db;
#[cfg(all(feature = "db", not(feature = "sqlite")))]
pub(crate) mod replay_db;
#[cfg(all(feature = "db", not(feature = "sqlite")))]
pub(crate) mod wire;

#[cfg(feature = "test-support")]
pub use capture::with_capture_scope;
pub use capture::{
    CaptureHandle, CaptureLayer, CaptureScope, CaptureSettings, CapturedClientIdentity, DbBuffer,
    current_scope, is_valid_scope_id, scope_by_id,
};
pub use clock::{RecordingClock, ReplayClock};
pub use persist::{CapsuleRef, capsule_dir, load_capsule, persist};
/// The recording pool, re-exported for tests that drive capture against a live
/// database without reaching into the submodule path.
#[cfg(all(feature = "test-support", feature = "db", not(feature = "sqlite")))]
pub use record_db::{build_recording_pool, maybe_capture_pool_provider};
/// Why database capture cannot record a given URL, re-exported for tests and
/// for `autumn doctor`-style preflight checks.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
pub use record_db::{capture_unavailable_reason, note_db_capture_unavailable};
pub use replay::{
    Divergence, DivergenceKind, DivergenceLog, EXIT_DIVERGED, EXIT_REFUSED, EXIT_REPRODUCED,
    ReplayOutcome, TapeProgress, Verdict, execute, print_refusal, print_verdict, refusal_exit_code,
    refusal_reason,
};
#[cfg(all(feature = "db", not(feature = "sqlite")))]
pub use replay_db::{StubServer, pool_from_capsule, replica_pool_from_capsule};

/// What a capsule says when the database backend has no wire capture at all.
///
/// `SQLite` connections carry no `PostgreSQL` protocol to tee (F18), so a
/// request that used one produces a capsule with no database tape.
#[cfg(all(feature = "db", feature = "sqlite"))]
pub const BACKEND_CAPTURE_NOTE: &str = "database capture is not available on the sqlite backend: this request's queries are absent \
     from the tape";

/// Record that the in-flight request used a database this build cannot record.
///
/// The `PostgreSQL` builds' equivalent is
/// `record_db::note_db_capture_unavailable`; this is the `sqlite` sibling, and
/// like it the capsule is **marked truncated** as well as noted — a capsule
/// that is missing the request's database effects must be refused by replay,
/// not presented as replayable.
#[cfg(all(feature = "db", feature = "sqlite"))]
pub fn note_backend_capture_gap() {
    if let Some(scope) = current_scope() {
        scope.note(BACKEND_CAPTURE_NOTE);
        scope.mark_truncated();
    }
}
pub use schema::{
    AppInfo, BindValue, CAPSULE_FORMAT_VERSION, Capsule, CapsuleBody, CapsuleDb, CapsuleError,
    CapsuleOutcome, CapsuleRequest, ConnectionTape, Exchange, ExchangeProtocol,
};

/// Build the capture settings the layer and the persistence path share.
#[must_use]
pub fn settings_from_config(config: &crate::config::AutumnConfig) -> CaptureSettings {
    CaptureSettings {
        dir: config.failure_capture.dir.clone(),
        max_body_bytes: config.failure_capture.max_body_bytes,
        max_capsule_bytes: config.failure_capture.max_capsule_bytes,
        max_capsules: config.failure_capture.max_capsules,
        app_name: Some(config.telemetry.service_name.clone()),
        profile: config.profile.clone(),
    }
}
