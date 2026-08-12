//! Replaying a capsule against a rebuilt application and judging the result.
//!
//! Everything the recorded request touched is served from the capsule — the
//! clock from [`ReplayClock`](crate::capsule::clock::ReplayClock), the database
//! from the stub server in `capsule::replay_db` — so the only remaining
//! variable is the code. [`execute`] rebuilds the recorded `http::Request`,
//! drives it through the router, and compares what came back with what the
//! capsule recorded.
//!
//! Three verdicts are possible:
//!
//! * [`Verdict::Reproduced`] — same outcome, no database divergence. The bug is
//!   still there (or the capsule records a fixed one and you are looking at a
//!   regression test).
//! * [`Verdict::Diverged`] — the replayed code asked the database something the
//!   recording never asked. The tape cannot answer it, so the run is not a fair
//!   comparison: the code has changed underneath the capsule. A divergence wins
//!   over a matching status, because a status that matches by luck while the
//!   queries differ is not a reproduction.
//! * [`Verdict::Mismatch`] — the database tape lined up but the outcome did
//!   not. Usually what you want to see after a fix.
//!
//! This module deliberately holds no database types, so it compiles in a build
//! without the `db` feature; [`DivergenceLog`] is a plain shared buffer the
//! stub server writes into.

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

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode, Uri, Version};
use futures::FutureExt as _;
use serde::Serialize;
use tower::ServiceExt as _;

use crate::capsule::clock::ReplayClock;
use crate::capsule::schema::{Capsule, CapsuleBody, CapsuleOutcome, CapsuleRequest};

/// Process exit code for a faithful reproduction.
pub const EXIT_REPRODUCED: i32 = 0;
/// Process exit code for a divergent or mismatched replay.
pub const EXIT_DIVERGED: i32 = 1;
/// Process exit code for a capsule this build refuses to replay.
pub const EXIT_REFUSED: i32 = 2;

/// Largest response body read back for the verdict's error message.
const MAX_BODY_PEEK: usize = 64 * 1024;

// ── Divergences ─────────────────────────────────────────────────────────────

/// Why the replayed database traffic did not line up with the tape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceKind {
    /// The tape held no exchange for the statement at all.
    UnrecordedQuery,
    /// The next recorded exchange carried different SQL.
    SqlMismatch,
    /// The SQL matched but an unmasked bind parameter did not.
    BindMismatch,
    /// The connection ran past the end of its recorded exchanges.
    TapeExhausted,
    /// A prepared statement was described that the tape holds no metadata for.
    UnknownStatement,
}

impl DivergenceKind {
    /// Short label used in the human summary.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::UnrecordedQuery => "unrecorded query",
            Self::SqlMismatch => "sql mismatch",
            Self::BindMismatch => "bind mismatch",
            Self::TapeExhausted => "tape exhausted",
            Self::UnknownStatement => "unknown statement",
        }
    }
}

/// One place where the replayed run asked for something the capsule cannot
/// answer.
#[derive(Debug, Clone, Serialize)]
pub struct Divergence {
    /// What went wrong.
    pub kind: DivergenceKind,
    /// Recorder-assigned id of the connection it happened on.
    pub connection: u64,
    /// Position in the connection's recorded exchange list.
    pub exchange_index: usize,
    /// SQL the tape expected next, when there was one.
    pub expected_sql: Option<String>,
    /// SQL the replayed code actually sent.
    pub actual_sql: String,
    /// Human-readable explanation, safe to print.
    pub detail: String,
}

/// Shared, append-only record of every divergence a replay run produced.
///
/// The stub server writes into it from the connection tasks while the router
/// runs, so it is an `Arc`-shared mutex rather than a return value.
#[derive(Debug, Default)]
pub struct DivergenceLog {
    entries: Mutex<Vec<Divergence>>,
}

impl DivergenceLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a divergence.
    ///
    /// A poisoned lock is swallowed rather than propagated: a replay run that
    /// loses one divergence line is still worth finishing, and the connection
    /// task has nowhere to report to.
    pub fn record(&self, divergence: Divergence) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.push(divergence);
        }
    }

    /// `true` when the run stayed on the tape.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_ok_and(|entries| entries.is_empty())
    }

    /// How many divergences were recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |entries| entries.len())
    }

    /// A snapshot of everything recorded so far.
    #[must_use]
    pub fn entries(&self) -> Vec<Divergence> {
        self.entries
            .lock()
            .map(|entries| entries.clone())
            .unwrap_or_default()
    }
}

// ── Verdict ─────────────────────────────────────────────────────────────────

/// The judgement a replay run reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Same outcome, no divergence.
    Reproduced,
    /// The replayed code left the tape.
    Diverged,
    /// The tape held, the outcome did not.
    Mismatch,
}

impl Verdict {
    /// Process exit code this verdict maps to.
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Reproduced => EXIT_REPRODUCED,
            Self::Diverged | Self::Mismatch => EXIT_DIVERGED,
        }
    }

    /// Lower-case label used in the JSON verdict and the human summary.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Reproduced => "reproduced",
            Self::Diverged => "diverged",
            Self::Mismatch => "mismatch",
        }
    }
}

/// Everything a replay run observed.
#[derive(Debug, Clone, Serialize)]
pub struct ReplayOutcome {
    /// The judgement.
    pub verdict: Verdict,
    /// What the capsule recorded.
    pub expected: CapsuleOutcome,
    /// What the replayed run produced.
    pub actual: CapsuleOutcome,
    /// Database divergences, in the order they happened.
    pub divergences: Vec<Divergence>,
    /// Non-fatal observations worth printing (clock over-reads, redaction
    /// limits, version drift).
    pub warnings: Vec<String>,
}

// ── Driver ──────────────────────────────────────────────────────────────────

/// Replay `capsule` against `router` and judge the result.
///
/// `divergences` must be the same log the replay database pool was built with
/// (see `capsule::replay_db::pool_from_capsule`) — it is read *after* the
/// router has finished, so every query the handler made is already in it.
/// `clock` is the [`ReplayClock`] installed on the rebuilt state, when there is
/// one; it is only read for the over-read warning.
///
/// The router is driven inside `catch_unwind`, so a handler that panics without
/// a panic-catching middleware is *compared* against a recorded panic rather
/// than aborting the replay process.
pub async fn execute(
    router: axum::Router,
    capsule: &Capsule,
    divergences: Arc<DivergenceLog>,
    clock: Option<&ReplayClock>,
) -> ReplayOutcome {
    let mut warnings = Vec::new();
    version_warnings(capsule, &mut warnings);

    let actual = match rebuild_request(&capsule.request, &mut warnings) {
        Ok(request) => drive(router, request).await,
        Err(reason) => {
            warnings.push(format!(
                "the recorded request could not be rebuilt: {reason}"
            ));
            CapsuleOutcome::Status {
                code: 0,
                message: reason,
                problem_type: None,
            }
        }
    };

    if let Some(clock) = clock {
        let over_reads = clock.over_reads();
        if over_reads > 0 {
            warnings.push(format!(
                "the replayed handler read the clock {over_reads} more time(s) than the recording \
                 did; the last recorded reading was repeated, so times after that point are not \
                 faithful"
            ));
        }
    }

    redaction_warning(capsule, &actual, &mut warnings);

    let divergences = divergences.entries();
    let verdict = if divergences.is_empty() {
        if outcomes_match(&capsule.outcome, &actual) {
            Verdict::Reproduced
        } else {
            Verdict::Mismatch
        }
    } else {
        Verdict::Diverged
    };

    ReplayOutcome {
        verdict,
        expected: capsule.outcome.clone(),
        actual,
        divergences,
        warnings,
    }
}

/// Drive one rebuilt request through the router, capturing an escaping panic.
async fn drive(router: axum::Router, request: Request<Body>) -> CapsuleOutcome {
    let call = AssertUnwindSafe(router.oneshot(request));
    match call.catch_unwind().await {
        Ok(Ok(response)) => outcome_from_response(response).await,
        // `Router`'s error type is `Infallible`, but the service contract still
        // has an error arm; treat it as a zero-status failure rather than
        // unwrapping.
        Ok(Err(error)) => CapsuleOutcome::Status {
            code: 0,
            message: format!("the router failed to answer: {error}"),
            problem_type: None,
        },
        Err(payload) => CapsuleOutcome::Panic {
            status: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
            payload: format_panic_payload(payload.as_ref()),
            backtrace: None,
        },
    }
}

/// Describe the response the way the capture path describes it, so the two are
/// directly comparable: message and problem type come from the
/// `AutumnErrorInfo` the error pipeline stashes in the extensions, exactly as
/// `reporting::report_response` reads them.
async fn outcome_from_response(response: axum::response::Response) -> CapsuleOutcome {
    let status = response.status();
    let info = response
        .extensions()
        .get::<crate::middleware::exception_filter::AutumnErrorInfo>();
    let (message, problem_type) = info.map_or_else(
        || {
            (
                status
                    .canonical_reason()
                    .unwrap_or("server error")
                    .to_owned(),
                None,
            )
        },
        |info| (info.message.clone(), info.problem_type.map(str::to_owned)),
    );
    // Draining the body keeps a streaming handler from being judged before it
    // has actually produced anything.
    let _ = axum::body::to_bytes(response.into_body(), MAX_BODY_PEEK).await;
    CapsuleOutcome::Status {
        code: status.as_u16(),
        message,
        problem_type,
    }
}

/// Same downcast ladder `reporting::format_panic_payload` uses.
fn format_panic_payload(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "handler panicked".to_owned())
}

/// Rebuild the recorded request.
fn rebuild_request(
    recorded: &CapsuleRequest,
    warnings: &mut Vec<String>,
) -> Result<Request<Body>, String> {
    let method = Method::from_bytes(recorded.method.as_bytes())
        .map_err(|error| format!("method {:?} is not valid: {error}", recorded.method))?;
    let uri: Uri = recorded
        .uri
        .parse()
        .map_err(|error| format!("uri {:?} is not valid: {error}", recorded.uri))?;

    let body = match &recorded.body {
        CapsuleBody::Absent => Body::empty(),
        CapsuleBody::Text(text) => Body::from(text.clone()),
        CapsuleBody::Base64(encoded) => {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded.as_bytes())
                .map_err(|error| format!("the recorded body is not valid base64: {error}"))?;
            Body::from(bytes)
        }
        CapsuleBody::Skipped { declared_len } => {
            warnings.push(format!(
                "the recorded body was larger than the capture cap ({}) and was never read, so \
                 the replayed request is sent with an empty body",
                declared_len.map_or_else(
                    || "length unknown".to_owned(),
                    |len| format!(
                        "{len} \
                     bytes declared"
                    )
                )
            ));
            Body::empty()
        }
    };

    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .version(parse_version(&recorded.http_version));
    for (name, value) in &recorded.headers {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            warnings.push(format!("dropped unparseable recorded header name {name:?}"));
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            warnings.push(format!(
                "dropped unparseable recorded header value for {name}"
            ));
            continue;
        };
        builder = builder.header(name, value);
    }
    builder
        .body(body)
        .map_err(|error| format!("the rebuilt request is not valid: {error}"))
}

/// `http::Version` as `redact` formats it (`{:?}`), back into a `Version`.
fn parse_version(text: &str) -> Version {
    match text {
        "HTTP/0.9" => Version::HTTP_09,
        "HTTP/1.0" => Version::HTTP_10,
        "HTTP/2.0" => Version::HTTP_2,
        "HTTP/3.0" => Version::HTTP_3,
        _ => Version::HTTP_11,
    }
}

/// Whether the replayed outcome counts as the recorded one.
///
/// A recorded panic compares by payload when the replayed panic also escaped
/// (substring either way, so a payload the recorder truncated still matches),
/// and by status when a panic-catching middleware turned one side into a plain
/// response — which is the ordinary case for an Autumn router.
fn outcomes_match(expected: &CapsuleOutcome, actual: &CapsuleOutcome) -> bool {
    match (expected, actual) {
        (
            CapsuleOutcome::Status { code: expected, .. },
            CapsuleOutcome::Status { code: actual, .. },
        ) => expected == actual,
        (
            CapsuleOutcome::Panic {
                payload: expected, ..
            },
            CapsuleOutcome::Panic {
                payload: actual, ..
            },
        ) => expected == actual || actual.contains(expected) || expected.contains(actual),
        (CapsuleOutcome::Panic { status, .. }, CapsuleOutcome::Status { code, .. })
        | (CapsuleOutcome::Status { code, .. }, CapsuleOutcome::Panic { status, .. }) => {
            status == code
        }
    }
}

/// Warn when the capsule came from a different build (F23, soft half).
fn version_warnings(capsule: &Capsule, warnings: &mut Vec<String>) {
    let running = env!("CARGO_PKG_VERSION");
    if capsule.autumn_version != running {
        warnings.push(format!(
            "the capsule was recorded by autumn-web {} but this build is {running}; a difference \
             in framework behaviour will show up as a mismatch that is not your application's",
            capsule.autumn_version
        ));
    }
    if capsule.truncated {
        warnings.push(
            "the capsule is truncated: recording stopped at a size cap, so its tape is \
             incomplete"
                .to_owned(),
        );
    }
}

/// F16: an authenticated route replayed without its credentials answers 401/403
/// where the recording answered 5xx. Say so, rather than leaving the operator
/// to work out why a reproduction failed.
fn redaction_warning(capsule: &Capsule, actual: &CapsuleOutcome, warnings: &mut Vec<String>) {
    let CapsuleOutcome::Status { code: actual, .. } = actual else {
        return;
    };
    if *actual != 401 && *actual != 403 {
        return;
    }
    let recorded_server_error = match &capsule.outcome {
        CapsuleOutcome::Status { code, .. } => (500..600).contains(code),
        CapsuleOutcome::Panic { .. } => true,
    };
    if !recorded_server_error {
        return;
    }
    let credential_redacted = capsule.request.redacted_keys.iter().any(|key| {
        let key = key.to_ascii_lowercase();
        key.starts_with("header:authorization")
            || key.starts_with("header:cookie")
            || key.starts_with("header:proxy-authorization")
    });
    if !credential_redacted {
        return;
    }
    warnings.push(format!(
        "the replay answered {actual} where the recording answered a server error, and the \
         capsule's credentials were masked by redaction (`{}`): authenticated routes are not \
         faithfully replayable from a capsule — re-record against an unauthenticated route, or \
         accept that the replay stops at the auth layer",
        capsule.request.redacted_keys.join("`, `")
    ));
}

// ── Refusal and reporting ───────────────────────────────────────────────────

/// Reason this build refuses to replay `capsule`, if any.
///
/// A refusal is not a verdict: nothing was run, so the caller should
/// [`print_refusal`] and exit [`EXIT_REFUSED`] rather than reporting a
/// mismatch. A format-version mismatch is refused earlier still, by
/// [`Capsule::from_json`](crate::capsule::schema::Capsule::from_json).
#[must_use]
pub fn refusal_reason(capsule: &Capsule) -> Option<String> {
    if capsule.truncated {
        return Some(
            "the capsule is truncated — recording hit a size cap partway through, so its \
             database tape and clock readings are incomplete and a replay would report \
             divergences that never happened. Raise `[failure_capture] max_capsule_bytes` and \
             re-record."
                .to_owned(),
        );
    }
    None
}

/// Exit code for a capsule this build refuses to replay.
#[must_use]
pub const fn refusal_exit_code() -> i32 {
    EXIT_REFUSED
}

/// Print a refusal — machine-readable on stdout, human-readable on stderr —
/// and return the process exit code ([`EXIT_REFUSED`]).
#[must_use]
pub fn print_refusal(reason: &str, capsule_path: &Path) -> i32 {
    let document = serde_json::json!({
        "verdict": "refused",
        "capsule": capsule_path.display().to_string(),
        "reason": reason,
    });
    println!("{document}");
    eprintln!("REFUSED  {}", capsule_path.display());
    eprintln!("  {reason}");
    EXIT_REFUSED
}

/// Print a replay verdict — JSON on stdout, a human summary on stderr — and
/// return the process exit code.
///
/// Exit codes: `0` reproduced, `1` diverged or mismatched, `2` refused (see
/// [`print_refusal`]).
#[must_use]
pub fn print_verdict(outcome: &ReplayOutcome, capsule_path: &Path) -> i32 {
    let document = serde_json::json!({
        "verdict": outcome.verdict.label(),
        "capsule": capsule_path.display().to_string(),
        "expected": outcome.expected,
        "actual": outcome.actual,
        "divergences": outcome.divergences,
        "warnings": outcome.warnings,
    });
    println!("{document}");

    eprintln!(
        "{}  {}",
        outcome.verdict.label().to_uppercase(),
        capsule_path.display()
    );
    eprintln!("  expected: {}", describe_outcome(&outcome.expected));
    eprintln!("  actual:   {}", describe_outcome(&outcome.actual));
    if !outcome.divergences.is_empty() {
        eprintln!("  database divergences ({}):", outcome.divergences.len());
        for divergence in &outcome.divergences {
            eprintln!(
                "    [{}] connection {} exchange {}: {}",
                divergence.kind.label(),
                divergence.connection,
                divergence.exchange_index,
                divergence.detail
            );
        }
    }
    for warning in &outcome.warnings {
        eprintln!("  warning: {warning}");
    }
    outcome.verdict.exit_code()
}

/// One-line human rendering of an outcome.
fn describe_outcome(outcome: &CapsuleOutcome) -> String {
    match outcome {
        CapsuleOutcome::Status {
            code,
            message,
            problem_type,
        } => problem_type.as_ref().map_or_else(
            || format!("{code} {message}"),
            |problem_type| format!("{code} {message} ({problem_type})"),
        ),
        CapsuleOutcome::Panic {
            status, payload, ..
        } => format!("{status} panic: {payload}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: u16) -> CapsuleOutcome {
        CapsuleOutcome::Status {
            code,
            message: "boom".to_owned(),
            problem_type: None,
        }
    }

    fn panic_outcome(payload: &str) -> CapsuleOutcome {
        CapsuleOutcome::Panic {
            status: 500,
            payload: payload.to_owned(),
            backtrace: None,
        }
    }

    fn fixture(outcome: CapsuleOutcome) -> Capsule {
        Capsule {
            format_version: crate::capsule::schema::CAPSULE_FORMAT_VERSION,
            id: "fixture".to_owned(),
            captured_at: chrono::Utc::now(),
            autumn_version: env!("CARGO_PKG_VERSION").to_owned(),
            app: crate::capsule::schema::AppInfo::default(),
            request: CapsuleRequest {
                method: "GET".to_owned(),
                uri: "/orders".to_owned(),
                route: None,
                http_version: "HTTP/1.1".to_owned(),
                headers: Vec::new(),
                body: CapsuleBody::Absent,
                redacted_keys: Vec::new(),
            },
            outcome,
            clock: Vec::new(),
            db: None,
            truncated: false,
            notes: Vec::new(),
        }
    }

    #[test]
    fn verdict_exit_codes_are_zero_one_two() {
        assert_eq!(Verdict::Reproduced.exit_code(), 0);
        assert_eq!(Verdict::Diverged.exit_code(), 1);
        assert_eq!(Verdict::Mismatch.exit_code(), 1);
        assert_eq!(refusal_exit_code(), 2);
    }

    #[test]
    fn same_status_reproduces_and_different_status_mismatches() {
        assert!(outcomes_match(&status(500), &status(500)));
        assert!(!outcomes_match(&status(500), &status(503)));
    }

    #[test]
    fn a_caught_panic_matches_the_recorded_panic_status() {
        assert!(outcomes_match(&panic_outcome("boom"), &status(500)));
        assert!(!outcomes_match(&panic_outcome("boom"), &status(503)));
        assert!(outcomes_match(
            &panic_outcome("boom"),
            &panic_outcome("boom")
        ));
        assert!(outcomes_match(
            &panic_outcome("boom"),
            &panic_outcome("boom at src/lib.rs:1")
        ));
        assert!(!outcomes_match(
            &panic_outcome("boom"),
            &panic_outcome("something else")
        ));
    }

    #[test]
    fn a_truncated_capsule_is_refused() {
        let mut capsule = fixture(status(500));
        assert!(refusal_reason(&capsule).is_none());
        capsule.truncated = true;
        let reason = refusal_reason(&capsule).expect("truncated capsules are refused");
        assert!(reason.contains("truncated"));
    }

    #[test]
    fn redaction_is_named_when_a_recorded_server_error_replays_as_401() {
        let mut capsule = fixture(status(500));
        capsule.request.redacted_keys = vec!["header:authorization".to_owned()];
        let mut warnings = Vec::new();
        redaction_warning(&capsule, &status(401), &mut warnings);
        assert_eq!(warnings.len(), 1, "expected one warning, got {warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("authenticated routes")));

        // No credential was masked: nothing to blame redaction for.
        let capsule = fixture(status(500));
        let mut warnings = Vec::new();
        redaction_warning(&capsule, &status(401), &mut warnings);
        assert!(warnings.is_empty(), "unexpected warning: {warnings:?}");

        // The recording itself was a 401: not a redaction artefact.
        let mut capsule = fixture(status(401));
        capsule.request.redacted_keys = vec!["header:authorization".to_owned()];
        let mut warnings = Vec::new();
        redaction_warning(&capsule, &status(401), &mut warnings);
        assert!(warnings.is_empty(), "unexpected warning: {warnings:?}");
    }

    #[test]
    fn the_recorded_request_is_rebuilt_verbatim() {
        let mut capsule = fixture(status(500));
        capsule.request.method = "POST".to_owned();
        capsule.request.uri = "/orders?page=2".to_owned();
        capsule.request.headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        capsule.request.body = CapsuleBody::Text("{\"a\":1}".to_owned());

        let mut warnings = Vec::new();
        let request = rebuild_request(&capsule.request, &mut warnings).expect("request rebuilds");
        assert_eq!(request.method(), Method::POST);
        assert_eq!(
            request.uri().path_and_query().map(ToString::to_string),
            Some("/orders?page=2".to_owned())
        );
        assert_eq!(
            request
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn a_skipped_body_warns_instead_of_pretending() {
        let mut capsule = fixture(status(500));
        capsule.request.body = CapsuleBody::Skipped {
            declared_len: Some(9_000_000),
        };
        let mut warnings = Vec::new();
        rebuild_request(&capsule.request, &mut warnings).expect("request rebuilds");
        assert!(
            warnings.iter().any(|w| w.contains("empty body")),
            "expected a skipped-body warning, got {warnings:?}"
        );
    }

    #[test]
    fn the_divergence_log_collects_across_clones() {
        let log = Arc::new(DivergenceLog::new());
        assert!(log.is_empty());
        Arc::clone(&log).record(Divergence {
            kind: DivergenceKind::UnrecordedQuery,
            connection: 3,
            exchange_index: 0,
            expected_sql: None,
            actual_sql: "SELECT 1".to_owned(),
            detail: "nothing recorded".to_owned(),
        });
        assert_eq!(log.len(), 1);
        assert!(!log.is_empty());
        assert_eq!(
            log.entries().first().map(|entry| entry.actual_sql.clone()),
            Some("SELECT 1".to_owned())
        );
    }

    #[tokio::test]
    async fn a_panicking_router_is_captured_not_propagated() {
        async fn boom() -> &'static str {
            panic!("kaboom in handler")
        }
        let router = axum::Router::new().route("/boom", axum::routing::get(boom));
        let mut capsule = fixture(panic_outcome("kaboom in handler"));
        capsule.request.uri = "/boom".to_owned();
        let outcome = execute(router, &capsule, Arc::new(DivergenceLog::new()), None).await;
        assert_eq!(outcome.verdict, Verdict::Reproduced, "{outcome:?}");
    }
}
