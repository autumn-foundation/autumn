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

pub mod capture;
pub mod clock;
pub mod persist;
pub mod redact;
pub mod schema;

// DB wire submodules (PostgreSQL only; the sqlite backend has no wire capture).
#[cfg(all(feature = "db", not(feature = "sqlite")))]
pub(crate) mod wire;

pub use capture::{
    CAPSULE_SCOPE, CaptureHandle, CaptureLayer, CaptureScope, CaptureSettings, DbBuffer,
    current_scope, db_capture_enabled, install_from_config, is_valid_scope_id, scope_by_id,
};
pub use clock::{RecordingClock, ReplayClock};
pub use persist::{CapsuleRef, capsule_dir, load_capsule, persist};
pub use schema::{
    BindValue, CAPSULE_FORMAT_VERSION, Capsule, CapsuleBody, CapsuleDb, CapsuleError,
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
