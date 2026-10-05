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
//!   recording never asked, *or* left part of the recording unasked. The tape
//!   cannot answer the first and the second is not a reproduction either, so
//!   the run is not a fair comparison: the code has changed underneath the
//!   capsule. A divergence wins over a matching status, because a status that
//!   matches by luck while the queries differ is not a reproduction.
//! * [`Verdict::Mismatch`] — the database tape lined up but the outcome did
//!   not. Usually what you want to see after a fix.
//!
//! This module deliberately holds no database types, so it compiles in a build
//! without the `db` feature; [`DivergenceLog`] is a plain shared buffer the
//! stub server writes into.

// Replay-time module (offline `autumn replay` runs, never the serving path);
// kept panic-averse with the same deny set, but deliberately outside the
// request-path panic-gate manifest — see CONTRIBUTING.md.
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode, Uri, Version};
use base64::Engine as _;
use futures::FutureExt as _;
use serde::Serialize;
use tower::ServiceExt as _;

use crate::capsule::clock::ReplayClock;
use crate::capsule::schema::{
    Capsule, CapsuleBody, CapsuleOutcome, CapsuleRequest, ConnectionTape,
};

/// Process exit code for a faithful reproduction.
pub const EXIT_REPRODUCED: i32 = 0;
/// Process exit code for a divergent or mismatched replay.
pub const EXIT_DIVERGED: i32 = 1;
/// Process exit code for a capsule this build refuses to replay.
pub const EXIT_REFUSED: i32 = 2;

/// Largest response body read back for the verdict's error message.
const MAX_BODY_PEEK: usize = 64 * 1024;

/// Longest the verdict waits for a replayed response body to finish.
///
/// The request timeout is deliberately cleared in replay mode, and a route
/// whose failure was *fixed* may now stream a body that never ends (an SSE
/// endpoint, say) — without a deadline of its own the drain would hang and
/// `autumn replay` would never print a verdict. Judging a still-streaming
/// response after this long is sound: the status and error identity are in
/// the head, which has already arrived.
const BODY_DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Drain up to [`MAX_BODY_PEEK`] of a replayed response body, giving up —
/// without failing the verdict — when it does not complete in time.
async fn drain_body(body: Body) {
    let _ = tokio::time::timeout(
        BODY_DRAIN_DEADLINE,
        axum::body::to_bytes(body, MAX_BODY_PEEK),
    )
    .await;
}

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
    /// The run finished with recorded exchanges left unasked on a connection.
    UnconsumedExchanges,
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
            Self::UnconsumedExchanges => "unconsumed exchanges",
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

/// How much of one recorded connection tape the replayed run actually asked
/// for.
///
/// A replay is only a reproduction if it *follows* the recording, and a
/// divergence log alone cannot see that: it only hears about statements the run
/// issued. A run that returns the recorded 500 without ever touching the
/// database issues nothing, so it would look flawless. The cursor lives here,
/// behind an atomic, because the stub server tasks are detached over a duplex
/// pipe — the driver has to be able to read their progress *after* the response
/// has resolved.
///
/// Only the ordered `exchanges` are tracked. The keyed buckets (`prologue`,
/// `statements`, `catalog`) are re-askable metadata rather than effects: a warm
/// recorded connection carries entries a cold replayed one may legitimately
/// never need.
#[derive(Debug)]
pub struct TapeProgress {
    connection: u64,
    /// SQL of every recorded exchange, in recorded order.
    exchanges: Vec<String>,
    consumed: AtomicUsize,
}

impl TapeProgress {
    /// A cursor over `exchanges` (their SQL, in order) for connection
    /// `connection`.
    #[must_use]
    pub const fn new(connection: u64, exchanges: Vec<String>) -> Self {
        Self {
            connection,
            exchanges,
            consumed: AtomicUsize::new(0),
        }
    }

    /// The recorder-assigned id of the connection this tape came from.
    #[must_use]
    pub const fn connection(&self) -> u64 {
        self.connection
    }

    /// How many recorded exchanges the run has consumed so far — which is also
    /// the index of the next one the tape expects.
    #[must_use]
    pub fn consumed(&self) -> usize {
        self.consumed.load(Ordering::SeqCst)
    }

    /// Mark the exchange at the current position as served.
    pub fn advance(&self) {
        self.consumed.fetch_add(1, Ordering::SeqCst);
    }

    /// How many recorded exchanges were never asked for.
    #[must_use]
    pub fn unconsumed(&self) -> usize {
        self.exchanges.len().saturating_sub(self.consumed())
    }

    /// The divergence the leftovers amount to, if there are any.
    fn leftover_divergence(&self) -> Option<Divergence> {
        let consumed = self.consumed();
        let first = self.exchanges.get(consumed)?;
        let total = self.exchanges.len();
        let left = total.saturating_sub(consumed);
        Some(Divergence {
            kind: DivergenceKind::UnconsumedExchanges,
            connection: self.connection,
            exchange_index: consumed,
            expected_sql: Some(first.clone()),
            actual_sql: String::new(),
            detail: format!(
                "the capsule recorded {total} exchange(s) on connection {} but the replayed run \
                 asked for only {consumed}; {left} recorded statement(s) were never issued, the \
                 first being {first:?} — the replayed code reached its outcome without following \
                 the recorded database effects",
                self.connection
            ),
        })
    }
}

/// Everything a replay run learns about its database traffic.
///
/// Two halves: an append-only record of every divergence the run produced, and
/// the per-connection consumption cursors that catch the divergences *nobody
/// issues* — a recorded exchange the run never asked for.
///
/// The stub server writes into it from the connection tasks while the router
/// runs, so it is an `Arc`-shared mutex rather than a return value.
#[derive(Debug, Default)]
pub struct DivergenceLog {
    entries: Mutex<Vec<Divergence>>,
    tapes: Mutex<Vec<Arc<TapeProgress>>>,
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

    /// Register a recorded tape and hand back its consumption cursor.
    ///
    /// Every tape in a capsule must be registered — including ones no
    /// connection ever claims, because a pool that opens fewer connections than
    /// the recording did leaves those recordings unfollowed just as surely as a
    /// half-read one does.
    pub fn register_tape(&self, tape: &ConnectionTape) -> Arc<TapeProgress> {
        let progress = Arc::new(TapeProgress::new(
            tape.id,
            tape.exchanges
                .iter()
                .map(|exchange| exchange.sql.clone())
                .collect(),
        ));
        if let Ok(mut tapes) = self.tapes.lock() {
            tapes.push(Arc::clone(&progress));
        }
        progress
    }

    /// One divergence per registered tape that still holds unasked exchanges.
    ///
    /// Read by [`execute`] once the router has finished; the stub tasks advance
    /// their cursors before writing each recorded response, so everything the
    /// run consumed is already counted by the time its response resolves.
    #[must_use]
    pub fn unconsumed(&self) -> Vec<Divergence> {
        self.tapes
            .lock()
            .map(|tapes| {
                tapes
                    .iter()
                    .filter_map(|tape| tape.leftover_divergence())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `true` when no divergence has been *recorded*.
    ///
    /// This is about statements the run issued; leftover recorded exchanges are
    /// reported separately by [`unconsumed`](Self::unconsumed), which [`execute`]
    /// folds in before reaching a verdict.
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
    /// Divergences on the non-database effect seams — outbound HTTP, job
    /// enqueues, cache, mail (#1634) — in the order they happened.
    #[serde(default)]
    pub effect_divergences: Vec<crate::capsule::effects::EffectDivergence>,
    /// Non-fatal observations worth printing (clock over-reads, redaction
    /// limits, version drift).
    pub warnings: Vec<String>,
}

// ── Fixtures ────────────────────────────────────────────────────────────────

/// Everything a capsule replays *through*: the clock, the entropy source and
/// the effect tape, built once from one capsule.
///
/// Bundling them is not tidiness — a replay is only deterministic if all three
/// come from the *same* capsule, and a single value makes that impossible to
/// get wrong. It is also the handle a generated regression test plugs into
/// [`TestApp`](crate::test::TestApp):
///
/// ```rust,ignore
/// let fixtures = ReplayFixtures::from_capsule(&capsule);
/// let router = TestApp::new()
///     .routes(routes![charge])
///     .with_clock(fixtures.clock())
///     .with_entropy(fixtures.entropy())
///     .build()
///     .into_router();
/// ```
#[derive(Debug, Clone)]
pub struct ReplayFixtures {
    clock: Arc<ReplayClock>,
    entropy: Arc<crate::capsule::entropy::ReplayEntropy>,
    effects: Arc<crate::capsule::effects::ReplayEffects>,
}

impl ReplayFixtures {
    /// Build the fixtures a capsule's recording describes.
    #[must_use]
    pub fn from_capsule(capsule: &Capsule) -> Self {
        // The fallback matters only for a capsule that recorded no readings at
        // all: `captured_at` is then the closest thing to the failure's own
        // wall time.
        let fallback = capsule
            .clock
            .first()
            .copied()
            .unwrap_or(capsule.captured_at);
        let clock = Arc::new(
            ReplayClock::new(capsule.clock.clone(), fallback).with_monotonic(
                capsule
                    .clock_monotonic_us
                    .iter()
                    .map(|micros| std::time::Duration::from_micros(*micros))
                    .collect(),
            ),
        );
        let entropy = Arc::new(crate::capsule::entropy::ReplayEntropy::new(
            capsule
                .effects
                .random
                .iter()
                .map(|draw| draw.bytes.clone())
                .collect(),
        ));
        let effects = Arc::new(crate::capsule::effects::ReplayEffects::new(
            capsule.effects.clone(),
        ));
        Self {
            clock,
            entropy,
            effects,
        }
    }

    /// The clock to install on the replayed application's state.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn crate::time::ClockSource> {
        Arc::clone(&self.clock) as Arc<dyn crate::time::ClockSource>
    }

    /// The entropy source to install on the replayed application's state.
    #[must_use]
    pub fn entropy(&self) -> Arc<dyn crate::entropy::Entropy> {
        Arc::clone(&self.entropy) as Arc<dyn crate::entropy::Entropy>
    }

    /// The effect tape the replayed request's seams are served from.
    #[must_use]
    pub fn effects(&self) -> Arc<crate::capsule::effects::ReplayEffects> {
        Arc::clone(&self.effects)
    }

    /// How many times the replayed run read the clock past the end of the
    /// recording.
    ///
    /// Exposed so a test can distinguish an over-read from an *under*-read:
    /// both produce a warning mentioning the clock, and asserting on the
    /// warning text alone would pass for either.
    #[must_use]
    pub fn clock_over_reads(&self) -> usize {
        self.clock.over_reads()
    }
}

// ── Driver ──────────────────────────────────────────────────────────────────

/// Replay `capsule` against `router` and judge the result.
///
/// `divergences` must be the same log the replay database pool was built with
/// (see `capsule::replay_db::pool_from_capsule`) — it is read *after* the
/// router has finished, so every query the handler made is already in it, and
/// so are the consumption cursors of every registered tape. Both halves count:
/// a statement the tape cannot answer is a divergence, and so is a recorded
/// exchange the run never asked for, because reaching the recorded outcome
/// without the recorded database traffic is not a reproduction.
/// `fixtures` must be the ones the router was built with
/// ([`ReplayFixtures::from_capsule`] on this same capsule): the clock and the
/// entropy source are read here for their over-read warnings, and the effect
/// tape is both installed for the run and drained for its divergences.
///
/// The router is driven inside `catch_unwind`, so a handler that panics without
/// a panic-catching middleware is *compared* against a recorded panic rather
/// than aborting the replay process.
pub async fn execute(
    router: axum::Router,
    capsule: &Capsule,
    divergences: Arc<DivergenceLog>,
    fixtures: &ReplayFixtures,
) -> ReplayOutcome {
    let mut warnings = Vec::new();
    version_warnings(capsule, &mut warnings);

    let actual = match rebuild_request(&capsule.request, &mut warnings) {
        Ok(request) => drive(router, request, fixtures.effects()).await,
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

    {
        let clock = fixtures.clock.as_ref();
        let over_reads = clock.over_reads();
        if over_reads > 0 {
            warnings.push(format!(
                "the replayed handler read the clock {over_reads} more time(s) than the recording \
                 did; the last recorded reading was repeated, so times after that point are not \
                 faithful"
            ));
        }
        let unconsumed = clock.unconsumed();
        if unconsumed > 0 {
            warnings.push(format!(
                "the replayed handler read the clock {unconsumed} fewer time(s) than the recording \
                 did — a time-dependent branch the recording took was not exercised, so treat a \
                 reproduced verdict with care"
            ));
        }
    }

    redaction_warning(capsule, &actual, &mut warnings);

    judge(capsule, actual, &divergences, fixtures, warnings)
}

/// Turn everything a finished replay observed into a verdict.
///
/// Shared by [`execute`] and [`execute_job`] so a request replay and a job
/// replay can never grade themselves differently.
fn judge(
    capsule: &Capsule,
    actual: CapsuleOutcome,
    divergences: &DivergenceLog,
    fixtures: &ReplayFixtures,
    mut warnings: Vec<String>,
) -> ReplayOutcome {
    entropy_warnings(fixtures.entropy.as_ref(), &mut warnings);
    scope_warning(capsule, fixtures, &mut warnings);

    // Statements the run issued that the tape could not answer, then recorded
    // statements the run never issued at all.
    let mut entries = divergences.entries();
    entries.extend(divergences.unconsumed());
    // The same two halves for the effect seams: `finish` returns the
    // divergences logged during the run *plus* every recorded effect the run
    // never asked for.
    let effect_entries = fixtures.effects.finish();
    let verdict = if entries.is_empty() && effect_entries.is_empty() {
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
        divergences: entries,
        effect_divergences: effect_entries,
        warnings,
    }
}

/// Warn when the replayed router never entered the scope that serves the
/// recorded clock and randomness.
///
/// That scope is established by
/// [`ReportingLayer`](crate::reporting::ReportingLayer). A router assembled
/// without it — a hand-built `axum::Router`, or a regression test whose factory
/// does not go through `TestApp` — runs on a clock and an entropy source that
/// serve stable values without consuming anything, and could otherwise report
/// `Reproduced` on a run that never met the recording's time or identifiers at
/// all. Only worth saying when the capsule actually recorded some.
fn scope_warning(capsule: &Capsule, fixtures: &ReplayFixtures, warnings: &mut Vec<String>) {
    if fixtures.effects.scope_entered() {
        return;
    }
    if capsule.clock.is_empty() && capsule.effects.random.is_empty() {
        return;
    }
    warnings.push(
        "the replayed router never entered the capsule's replay scope, so the recorded clock \
         readings and random draws were not served — the router was built without Autumn's \
         reporting layer (use `TestApp::build().into_router()`, or `autumn replay`, which \
         rebuilds the real one)"
            .to_owned(),
    );
}

/// Warn when the replayed run drew a different number of random values than
/// the recording did.
///
/// Symmetric with the clock warnings above, and for the same reason: an
/// identifier minted from an exhausted tape is not the one the failure used,
/// and a branch that no longer mints one never took the recorded path.
fn entropy_warnings(entropy: &crate::capsule::entropy::ReplayEntropy, warnings: &mut Vec<String>) {
    let over_draws = entropy.over_draws();
    if over_draws > 0 {
        warnings.push(format!(
            "the replayed handler drew randomness {over_draws} more time(s) than the recording \
             did; those draws were served as zero bytes, so identifiers minted after that point \
             are not the ones the failing request used"
        ));
    }
    let unconsumed = entropy.unconsumed();
    if unconsumed > 0 {
        warnings.push(format!(
            "the replayed handler drew randomness {unconsumed} fewer time(s) than the recording \
             did — a branch that minted an identifier in the failing run was not exercised, so \
             treat a reproduced verdict with care"
        ));
    }
}

/// How a job-entry capsule's job is dispatched during replay.
///
/// A boxed closure rather than a `JobHandler` fn pointer, so the caller can
/// capture the rebuilt `AppState` the handler needs without `execute_job`
/// having to know how the application assembles one.
pub type JobDispatch = Box<
    dyn FnOnce(
            serde_json::Value,
        ) -> std::pin::Pin<Box<dyn Future<Output = crate::AutumnResult<()>> + Send>>
        + Send,
>;

/// Replay a **job-entry** capsule by dispatching the recorded job.
///
/// The request-capsule sibling of [`execute`]. A job failure is not reachable
/// through the router — there is no request to drive — so the recorded payload
/// is handed straight to the job's handler, with the same clock, entropy and
/// effect tape a replayed request gets.
///
/// The replay-request scope is established here rather than by
/// `ReportingLayer`, and that is still symmetric with capture: a job execution
/// is recorded under a capture scope that wraps the *whole* run
/// (`capture::capture_job`), with no Tower stack in between.
///
/// # Errors
///
/// Never returns an error: a panicking handler is caught and compared against
/// a recorded panic, exactly as the request path does.
pub async fn execute_job(
    dispatch: JobDispatch,
    capsule: &Capsule,
    divergences: Arc<DivergenceLog>,
    fixtures: &ReplayFixtures,
) -> ReplayOutcome {
    let mut warnings = Vec::new();
    version_warnings(capsule, &mut warnings);

    let Some(job) = capsule.job.as_ref() else {
        warnings.push(
            "this capsule records a request, not a job; replay it through the router".to_owned(),
        );
        return ReplayOutcome {
            verdict: Verdict::Diverged,
            expected: capsule.outcome.clone(),
            actual: CapsuleOutcome::Status {
                code: 0,
                message: "not a job capsule".to_owned(),
                problem_type: None,
            },
            divergences: Vec::new(),
            effect_divergences: Vec::new(),
            warnings,
        };
    };

    let payload = job.payload.clone();
    let run = crate::capsule::effects::with_effect_tape(
        fixtures.effects(),
        crate::capsule::clock::with_replay_request_scope(async move {
            // The marker that says the recorded clock and entropy were actually
            // served is otherwise set only by `ReportingLayer`, which a direct
            // job dispatch never traverses — so without this every faithful job
            // replay warns that fixtures it *did* serve went unused.
            let _ = crate::capsule::effects::tape_active();
            dispatch(payload).await
        }),
    );
    let actual = match AssertUnwindSafe(run).catch_unwind().await {
        Ok(Ok(())) => CapsuleOutcome::Status {
            // A job that now succeeds is the "the bug is gone" shape; code 0
            // never collides with the 500 a recorded failure carries.
            code: 0,
            message: "the job succeeded".to_owned(),
            problem_type: None,
        },
        Ok(Err(error)) => CapsuleOutcome::Status {
            code: 500,
            // `message`, not `Display`: paired against the recorded
            // string, which `job::run_job_handler` also records this way.
            message: error.message(),
            problem_type: None,
        },
        Err(payload) => CapsuleOutcome::Panic {
            status: 500,
            // Byte-identical to what `job::format_job_panic` records, including
            // its non-string fallback: `outcomes_match` compares payloads
            // exactly, so a different spelling here would make every
            // non-string job panic replay as a `Mismatch`.
            payload: format_job_panic_payload(payload.as_ref()),
            backtrace: None,
        },
    };

    judge(capsule, actual, &divergences, fixtures, warnings)
}

/// Drive one rebuilt request through the router, capturing an escaping panic.
///
/// Installs the effect tape for the run. The scope that entitles the run to
/// *consume* recorded clock readings and random draws is established one layer
/// deeper, by [`ReportingLayer`](crate::reporting::ReportingLayer), so that it
/// lines up exactly with the boundary capture recorded through — see
/// [`crate::capsule::clock::with_replay_request_scope`]. Reads from anywhere
/// else during the replay (boot, or tasks the handler spawns) are served
/// non-consuming, mirroring the capture side, where only scope-carrying reads
/// were recorded.
async fn drive(
    router: axum::Router,
    request: Request<Body>,
    effects: Arc<crate::capsule::effects::ReplayEffects>,
) -> CapsuleOutcome {
    // Only the effect tape is installed here. The *consuming* scope for the
    // clock and the entropy source is established one layer deeper, by
    // `ReportingLayer`, so that it lines up exactly with the boundary capture
    // recorded through — see `clock::with_replay_request_scope`.
    let call = crate::capsule::effects::with_effect_tape(effects, router.oneshot(request));
    match AssertUnwindSafe(call).catch_unwind().await {
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
    // A panic the reporting layer caught and converted into a sanitized 500
    // still carries its identity in the extensions; describe it as the panic
    // it was, so it is compared against a recorded panic by payload rather
    // than letting any same-status response pass for it.
    if let Some(caught) = response.extensions().get::<crate::reporting::CaughtPanic>() {
        let payload = caught.payload.clone();
        drain_body(response.into_body()).await;
        return CapsuleOutcome::Panic {
            status: status.as_u16(),
            payload,
            backtrace: None,
        };
    }
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
    // has actually produced anything — but only up to a deadline, so a body
    // that never ends cannot hang the verdict.
    drain_body(response.into_body()).await;
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

/// The panic text a job execution records, reproduced exactly.
///
/// Mirrors `job::format_job_panic`; the two must not drift, which is what the
/// `job_panic_text_matches_the_capture_side` test pins.
fn format_job_panic_payload(payload: &(dyn Any + Send)) -> String {
    let detail = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&'static str>().copied())
        .unwrap_or("non-string panic payload");
    format!("job handler panicked: {detail}")
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
    // Restore client identity: the recorded *resolved* identity is
    // pre-inserted whole, and `TrustedProxiesLayer` honors a pre-existing
    // `ResolvedClientIdentity` rather than re-resolving — re-running trust
    // evaluation against a synthetic peer would (correctly) distrust the
    // recorded forwarded headers and settle on a different host and scheme
    // than the failing request saw. `ConnectInfo` is anchored too, for
    // anything reading the peer directly.
    if recorded.client_addr.is_some()
        || recorded.client_host.is_some()
        || recorded.client_scheme.is_some()
    {
        builder = builder.extension(crate::security::ResolvedClientIdentity {
            addr: recorded.client_addr,
            host: recorded.client_host.clone(),
            scheme: recorded.client_scheme.clone(),
        });
    }
    // The raw peer socket outranks a synthesized one: middleware and
    // handlers that inspect the peer directly (address *and* port) see what
    // the recording server saw. Capsules that predate `peer_addr` fall back
    // to anchoring the resolved client address with a zero port.
    if let Some(peer) = recorded.peer_addr {
        builder = builder.extension(axum::extract::ConnectInfo(peer));
    } else if let Some(addr) = recorded.client_addr {
        builder = builder.extension(axum::extract::ConnectInfo(std::net::SocketAddr::new(
            addr, 0,
        )));
    }
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
    // Non-UTF-8 header values travel base64-encoded so the JSON stays
    // diffable; restore the exact bytes — a placeholder here would hand the
    // handler different metadata than production saw.
    for (name, encoded) in &recorded.binary_headers {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            warnings.push(format!("dropped unparseable recorded header name {name:?}"));
            continue;
        };
        let value = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()
            .and_then(|bytes| HeaderValue::from_bytes(&bytes).ok());
        let Some(value) = value else {
            warnings.push(format!(
                "dropped undecodable recorded binary header value for {name}"
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
/// Two failures are the same failure when they have the same *identity*, not
/// merely the same status: a 500 whose message and problem type differ is a
/// different bug, and reporting it as a reproduction is exactly the wrong
/// answer for a tool whose job is telling you whether the bug is still there.
/// So a status outcome compares code, problem type **and** message. The
/// comparison is fair because both sides went through the same redaction: the
/// replayed request carries the capsule's `[FILTERED]` literals, so a message
/// that echoes request content echoes the same masked content.
///
/// A recorded panic compares by whole payload when the replayed panic also
/// escaped, and never across variants: a panic-catching middleware keeps the
/// panic's identity (`CaughtPanic`), so a cross-variant pair means the
/// failure *kind* changed.
fn outcomes_match(expected: &CapsuleOutcome, actual: &CapsuleOutcome) -> bool {
    match (expected, actual) {
        (
            CapsuleOutcome::Status {
                code: expected_code,
                message: expected_message,
                problem_type: expected_type,
            },
            CapsuleOutcome::Status {
                code: actual_code,
                message: actual_message,
                problem_type: actual_type,
            },
        ) => {
            expected_code == actual_code
                && expected_type == actual_type
                && expected_message == actual_message
        }
        (
            CapsuleOutcome::Panic {
                payload: expected, ..
            },
            CapsuleOutcome::Panic {
                payload: actual, ..
            },
        ) => panic_payloads_match(expected, actual),
        // A caught panic keeps its identity through the reporting layer (the
        // `CaughtPanic` response extension), so both sides of a genuinely
        // reproduced panic present as `Panic` above. A cross-variant pair
        // therefore means the failure *kind* changed — a fixed panic replaced
        // by an ordinary error, or the reverse — and a shared status code is
        // not a reproduction.
        (CapsuleOutcome::Panic { .. }, CapsuleOutcome::Status { .. })
        | (CapsuleOutcome::Status { .. }, CapsuleOutcome::Panic { .. }) => false,
    }
}

/// Whether two panic payloads describe the same panic.
fn panic_payloads_match(expected: &str, actual: &str) -> bool {
    // Whole-payload equality, deliberately: persistence never truncates a
    // panic payload (redaction only substitutes masked secrets, and the
    // replayed handler panics with the same `[FILTERED]` literals its request
    // carries), so a substring rule would let `database timeout while writing
    // the audit log` pass for a recorded `database timeout` — a different
    // panic wearing the old one's prefix.
    expected == actual
}

/// The one-line explanation for a verdict whose status lined up but whose
/// failure identity did not, so a reader is not left comparing two `500`s.
fn identity_mismatch_note(expected: &CapsuleOutcome, actual: &CapsuleOutcome) -> Option<String> {
    let (
        CapsuleOutcome::Status {
            code: expected_code,
            message: expected_message,
            problem_type: expected_type,
        },
        CapsuleOutcome::Status {
            code: actual_code,
            message: actual_message,
            problem_type: actual_type,
        },
    ) = (expected, actual)
    else {
        return None;
    };
    if expected_code != actual_code {
        return None;
    }
    if expected_type != actual_type {
        return Some(format!(
            "the status matched ({expected_code}) but the failure identity did not: the capsule \
             recorded problem type {expected_type:?} and the replay produced {actual_type:?}"
        ));
    }
    (expected_message != actual_message).then(|| {
        format!(
            "the status matched ({expected_code}) but the failure identity did not: the capsule \
             recorded {expected_message:?} and the replay produced {actual_message:?} — same \
             status, different failure"
        )
    })
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
            "the capsule is truncated: recording stopped before it was complete — its notes \
             say why"
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
            "the capsule is truncated — recording stopped before it was complete (a size cap, \
             an unrecordable connection, or a streaming response body), so a replay would \
             report divergences that never happened. The capsule's notes say exactly why; for \
             a size cap, raise `[failure_capture] max_capsule_bytes` and re-record."
                .to_owned(),
        );
    }
    if let CapsuleBody::Skipped { declared_len } = &capsule.request.body {
        // Replaying with an empty body would drive the handler with input the
        // failing request never had. The handler then rejects it, the verdict
        // reads `mismatch` — which the guide tells operators means "the bug is
        // gone" — and a live bug is quietly marked fixed. Missing input is a
        // refusal, not a verdict.
        let size = declared_len.map_or_else(
            || "its size was never declared".to_owned(),
            |len| format!("it declared {len} byte(s)"),
        );
        return Some(format!(
            "the capsule's request body was not recorded ({size}) — it was over \
             `[failure_capture] max_body_bytes`, or it declared a structure redaction could not \
             parse and mask. Replaying would send an empty body, so a handler that reads the \
             body would be judged on input the failing request never had. The capsule's notes \
             say which case this was; raise `max_body_bytes` and re-record if it was the cap."
        ));
    }
    // Effect data the handler *consumes* cannot tolerate a placeholder the way
    // compared data can. A response body and a cache hit are deserialized and
    // branched on, so `[FILTERED]` reaches the code as a literal — a number
    // field that no longer parses, a string that takes the wrong arm — and the
    // verdict would describe a run that never happened. The masking is not
    // reversible, so this is a refusal rather than a warning.
    let masked_input: Vec<&str> = capsule
        .request
        .redacted_keys
        .iter()
        .filter(|key| {
            key.starts_with("cache[")
                || key == &"tenant.id"
                || (key.starts_with("http[")
                    && (key.contains("].response_body")
                        || key.contains("].response_header")
                        || key.contains("].final_url")))
        })
        .map(String::as_str)
        .collect();
    if !masked_input.is_empty() {
        return Some(format!(
            "input the replayed run reads back was masked by `[log] filter_parameters` ({}). \
             An outbound response (its body, its headers, the URL a redirect landed on), a cache \
             hit and the resolved tenant are all handed to the code as data — parsed, \
             deserialized, branched on, scoped by — so unlike a compared field the `[FILTERED]` \
             placeholder cannot stand in for what was really there, and the run would be graded \
             on input production never returned. Redaction is not reversible: unfilter the field for this \
             route, or debug it from the recorded outcome instead.",
            masked_input.join(", ")
        ));
    }
    if capsule.job.is_some() {
        // The job payload is the *input* a job capsule replays on, and unlike
        // an effect it is handed to the handler verbatim: there is no wildcard
        // reading of `[FILTERED]` at the entry boundary, because the handler
        // parses the document rather than being compared against it. So a
        // masked field reaches the code as the literal placeholder — a
        // `serde` field that no longer deserializes, a branch that takes the
        // wrong arm — and the verdict describes a run that never happened.
        // The original bytes are gone by construction, so this is a refusal.
        let masked: Vec<&str> = capsule
            .request
            .redacted_keys
            .iter()
            .filter_map(|key| key.strip_prefix("job_entry."))
            .collect();
        if !masked.is_empty() {
            return Some(format!(
                "the job payload this capsule replays on had {} field(s) masked by \
                 `[log] filter_parameters` ({}). A job handler is handed its payload verbatim, \
                 so it would parse or branch on the `[FILTERED]` placeholder rather than the \
                 value production ran on, and the verdict would describe a run that never \
                 happened. Redaction is not reversible: unfilter the field for this job, or \
                 debug it from the recorded outcome instead.",
                masked.len(),
                masked.join(", ")
            ));
        }
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
    eprintln!("  {}", printable(reason));
    EXIT_REFUSED
}

/// Strip C0 control characters from a string that came out of a capsule before
/// it is written to a terminal.
///
/// Capsule text is production request data: an error message, a panic payload
/// or a SQL string can carry whatever a client sent, including ANSI escape
/// sequences that would repaint the operator's terminal, hide the rest of the
/// verdict, or forge a line of output. Newlines and tabs are kept — they are
/// ordinary in a SQL statement — and everything else in the C0 range, escape
/// included, becomes a visible placeholder. The JSON document on stdout is
/// untouched: it is data, and a consumer needs it verbatim.
fn printable(text: &str) -> String {
    if !text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return text.to_owned();
    }
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
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
        "effect_divergences": outcome.effect_divergences,
        "warnings": outcome.warnings,
    });
    println!("{document}");

    eprintln!(
        "{}  {}",
        outcome.verdict.label().to_uppercase(),
        capsule_path.display()
    );
    eprintln!(
        "  expected: {}",
        printable(&describe_outcome(&outcome.expected))
    );
    eprintln!(
        "  actual:   {}",
        printable(&describe_outcome(&outcome.actual))
    );
    if outcome.verdict == Verdict::Mismatch
        && let Some(note) = identity_mismatch_note(&outcome.expected, &outcome.actual)
    {
        eprintln!("  {}", printable(&note));
    }
    if !outcome.divergences.is_empty() {
        eprintln!("  database divergences ({}):", outcome.divergences.len());
        for divergence in &outcome.divergences {
            eprintln!(
                "    [{}] connection {} exchange {}: {}",
                divergence.kind.label(),
                divergence.connection,
                divergence.exchange_index,
                printable(&divergence.detail)
            );
        }
    }
    if !outcome.effect_divergences.is_empty() {
        // Without this the CLI would print `diverged` and then say nothing at
        // all about *what* diverged, for every seam outside the database.
        eprintln!(
            "  effect divergences ({}):",
            outcome.effect_divergences.len()
        );
        for divergence in &outcome.effect_divergences {
            eprintln!(
                "    [{} / {}] {}",
                divergence.seam.label(),
                divergence.kind.label(),
                printable(&divergence.detail)
            );
        }
    }
    for warning in &outcome.warnings {
        eprintln!("  warning: {}", printable(warning));
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
                binary_headers: Vec::new(),
                body: CapsuleBody::Absent,
                redacted_keys: Vec::new(),
                peer_addr: None,
                client_addr: None,
                client_host: None,
                client_scheme: None,
            },
            outcome,
            clock: Vec::new(),
            clock_monotonic_us: Vec::new(),
            db: None,
            db_roles: Vec::new(),
            truncated: false,
            notes: Vec::new(),
            effects: crate::capsule::schema::CapsuleEffects::default(),
            job: None,
        }
    }

    fn job_capsule(name: &str, payload: serde_json::Value, outcome: CapsuleOutcome) -> Capsule {
        let mut capsule = fixture(outcome);
        capsule.job = Some(crate::capsule::schema::CapsuleJob {
            name: name.to_owned(),
            payload,
        });
        capsule.request.method = "JOB".to_owned();
        capsule.request.uri = format!("/jobs/{name}");
        capsule
    }

    /// A job capsule replays by dispatching the job, not by driving a router —
    /// the other half of "a failure inside a job execution produces a
    /// job-scoped capsule replayable the same way".
    #[tokio::test]
    async fn a_job_capsule_reproduces_by_dispatching_the_recorded_job() {
        let capsule = job_capsule(
            "send_receipt",
            serde_json::json!({"order": 7}),
            CapsuleOutcome::Status {
                code: 500,
                message: "receipt 7 could not be sent".to_owned(),
                problem_type: None,
            },
        );
        let fixtures = ReplayFixtures::from_capsule(&capsule);
        let dispatch: crate::capsule::JobDispatch = Box::new(|payload| {
            Box::pin(async move {
                let order = payload.get("order").and_then(serde_json::Value::as_i64);
                Err(crate::AutumnError::internal_server_error_msg(format!(
                    "receipt {} could not be sent",
                    order.unwrap_or_default()
                )))
            })
        });

        let outcome = execute_job(
            dispatch,
            &capsule,
            Arc::new(DivergenceLog::new()),
            &fixtures,
        )
        .await;
        assert_eq!(outcome.verdict, Verdict::Reproduced, "{outcome:?}");
    }

    #[tokio::test]
    async fn a_fixed_job_replays_as_a_mismatch_not_a_reproduction() {
        let capsule = job_capsule(
            "send_receipt",
            serde_json::json!({"order": 7}),
            CapsuleOutcome::Status {
                code: 500,
                message: "receipt 7 could not be sent".to_owned(),
                problem_type: None,
            },
        );
        let fixtures = ReplayFixtures::from_capsule(&capsule);
        let dispatch: crate::capsule::JobDispatch = Box::new(|_| Box::pin(async { Ok(()) }));

        let outcome = execute_job(
            dispatch,
            &capsule,
            Arc::new(DivergenceLog::new()),
            &fixtures,
        )
        .await;
        assert_eq!(outcome.verdict, Verdict::Mismatch, "{outcome:?}");
    }

    /// A panicking job is compared against a recorded panic by payload, and the
    /// two sides must spell it identically or every job panic mismatches.
    #[tokio::test]
    async fn a_panicking_job_reproduces_a_recorded_panic() {
        let capsule = job_capsule(
            "send_receipt",
            serde_json::json!({}),
            CapsuleOutcome::Panic {
                status: 500,
                payload: "job handler panicked: receipts are down".to_owned(),
                backtrace: None,
            },
        );
        let fixtures = ReplayFixtures::from_capsule(&capsule);
        let dispatch: crate::capsule::JobDispatch =
            Box::new(|_| Box::pin(async { panic!("receipts are down") }));

        let outcome = execute_job(
            dispatch,
            &capsule,
            Arc::new(DivergenceLog::new()),
            &fixtures,
        )
        .await;
        assert_eq!(outcome.verdict, Verdict::Reproduced, "{outcome:?}");
    }

    /// The panic text the capture side writes and the text replay reconstructs
    /// are compared verbatim by `outcomes_match`, so they must not drift.
    #[test]
    fn job_panic_text_matches_the_capture_side() {
        let payload: Box<dyn Any + Send> = Box::new(42_u8);
        assert_eq!(
            format_job_panic_payload(payload.as_ref()),
            "job handler panicked: non-string panic payload"
        );
        let payload: Box<dyn Any + Send> = Box::new("boom".to_owned());
        assert_eq!(
            format_job_panic_payload(payload.as_ref()),
            "job handler panicked: boom"
        );
    }

    #[test]
    fn a_recorded_client_addr_is_restored_as_the_replayed_peer() {
        let mut recorded = crate::capsule::schema::test_support::request("GET", "/whoami");
        recorded.client_addr = Some(std::net::IpAddr::from([203, 0, 113, 9]));
        let mut warnings = Vec::new();
        let request = rebuild_request(&recorded, &mut warnings).expect("request rebuilds");
        let peer = request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .expect("the recorded client address must anchor the replayed peer");
        assert_eq!(peer.0.ip(), std::net::IpAddr::from([203, 0, 113, 9]));
        let identity = request
            .extensions()
            .get::<crate::security::ResolvedClientIdentity>()
            .expect("the full resolved identity must be restored");
        assert_eq!(
            identity.addr,
            Some(std::net::IpAddr::from([203, 0, 113, 9]))
        );

        let anonymous = crate::capsule::schema::test_support::request("GET", "/whoami");
        let request = rebuild_request(&anonymous, &mut warnings).expect("request rebuilds");
        assert!(
            request
                .extensions()
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .is_none(),
            "no recorded address, no synthetic peer"
        );
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
        // A replayed panic keeps its identity through the reporting layer via
        // the `CaughtPanic` extension, so a genuine reproduction presents as
        // Panic↔Panic; a bare status — any status — is a different failure.
        assert!(!outcomes_match(&panic_outcome("boom"), &status(500)));
        assert!(!outcomes_match(&panic_outcome("boom"), &status(503)));
        assert!(outcomes_match(
            &panic_outcome("boom"),
            &panic_outcome("boom")
        ));
        // Both sides format a payload the same way (a `&str`/`String`
        // downcast, no location suffix), so a superstring is a *different*
        // panic, not a tolerance case.
        assert!(!outcomes_match(
            &panic_outcome("boom"),
            &panic_outcome("boom at src/lib.rs:1")
        ));
        assert!(!outcomes_match(
            &panic_outcome("boom"),
            &panic_outcome("something else")
        ));
    }

    /// A failure's identity is more than its status code. Two different 500s
    /// are two different bugs, and calling the second one a reproduction of the
    /// first is the wrong answer from a tool whose whole job is telling you
    /// whether the bug is still there.
    #[test]
    fn a_matching_status_with_a_different_failure_is_a_mismatch() {
        let recorded = CapsuleOutcome::Status {
            code: 500,
            message: "order 42 has no shipping address".to_owned(),
            problem_type: Some("https://errors.example/db".to_owned()),
        };
        assert!(outcomes_match(&recorded, &recorded.clone()));

        let other_message = CapsuleOutcome::Status {
            code: 500,
            message: "connection pool exhausted".to_owned(),
            problem_type: Some("https://errors.example/db".to_owned()),
        };
        assert!(
            !outcomes_match(&recorded, &other_message),
            "a different failure with the same status is not a reproduction"
        );
        assert!(
            identity_mismatch_note(&recorded, &other_message)
                .is_some_and(|note| note.contains("failure identity")),
            "the verdict must explain that the status matched but the failure did not"
        );

        let other_type = CapsuleOutcome::Status {
            code: 500,
            message: "order 42 has no shipping address".to_owned(),
            problem_type: Some("https://errors.example/other".to_owned()),
        };
        assert!(!outcomes_match(&recorded, &other_type));
        assert!(identity_mismatch_note(&recorded, &other_type).is_some());

        // A genuinely different status is reported as such, not as an identity
        // difference.
        assert!(identity_mismatch_note(&recorded, &status(503)).is_none());
    }

    /// Panic payloads compare by whole-payload equality: persistence never
    /// truncates one, so a new panic that merely *contains* the recorded
    /// message — `database timeout while writing the audit log` for a
    /// recorded `database timeout` — is a different panic, not a
    /// reproduction.
    #[test]
    fn panic_payloads_compare_by_equality() {
        assert!(!outcomes_match(
            &panic_outcome(""),
            &panic_outcome("something else entirely")
        ));
        assert!(outcomes_match(&panic_outcome(""), &panic_outcome("")));
        assert!(!outcomes_match(
            &panic_outcome("handler panicked"),
            &panic_outcome("index out of bounds")
        ));
        assert!(outcomes_match(
            &panic_outcome("handler panicked"),
            &panic_outcome("handler panicked")
        ));
        assert!(
            !outcomes_match(
                &panic_outcome("database timeout"),
                &panic_outcome("database timeout while writing the audit log")
            ),
            "a superstring is a different panic wearing the old one's prefix"
        );
    }

    /// Capsule text is production request data. Printing it to a terminal
    /// verbatim would let a recorded ANSI escape repaint the operator's screen
    /// or forge a line of the verdict.
    #[test]
    fn control_characters_are_stripped_from_printed_capsule_text() {
        let scrubbed = printable("boom\u{1b}[2J\u{1b}[1;1HREPRODUCED  clean\u{7}");
        assert!(
            !scrubbed.contains('\u{1b}') && !scrubbed.contains('\u{7}'),
            "escape sequences must not reach the terminal, got {scrubbed:?}"
        );
        assert!(scrubbed.starts_with("boom"), "the text itself is kept");
        assert_eq!(
            printable("SELECT 1\n\tFROM t"),
            "SELECT 1\n\tFROM t",
            "newlines and tabs are ordinary in SQL and must survive"
        );
    }

    #[test]
    fn a_truncated_capsule_is_refused() {
        let mut capsule = fixture(status(500));
        assert!(refusal_reason(&capsule).is_none());
        capsule.truncated = true;
        let reason = refusal_reason(&capsule).expect("truncated capsules are refused");
        assert!(reason.contains("truncated"));
    }

    /// A body the capture never recorded is missing *input*, not a difference
    /// in behaviour. Replaying it empty and reporting `mismatch` would read as
    /// "the bug is gone" when nothing of the kind was established.
    #[test]
    fn a_capsule_whose_body_was_never_recorded_is_refused() {
        let mut capsule = fixture(status(500));
        assert!(refusal_reason(&capsule).is_none());

        capsule.request.body = CapsuleBody::Skipped {
            declared_len: Some(2_000_000),
        };
        let reason =
            refusal_reason(&capsule).expect("a capsule with an unrecorded body is refused");
        assert!(
            reason.contains("2000000"),
            "the refusal must say how big the body was: {reason}"
        );
        assert!(
            reason.contains("max_body_bytes"),
            "the refusal must point at the knob that caused it: {reason}"
        );

        // A skipped body with no declared length is refused just the same.
        capsule.request.body = CapsuleBody::Skipped { declared_len: None };
        assert!(refusal_reason(&capsule).is_some());

        // A body that was recorded, or genuinely absent, still replays.
        capsule.request.body = CapsuleBody::Text("{}".to_owned());
        assert!(refusal_reason(&capsule).is_none());
        capsule.request.body = CapsuleBody::Absent;
        assert!(refusal_reason(&capsule).is_none());
    }

    /// A job payload is *input*, handed to the handler verbatim. Unlike an
    /// effect it is never compared, so `[FILTERED]` has no wildcard reading
    /// here — the handler simply parses the placeholder.
    #[test]
    fn a_job_capsule_whose_payload_was_redacted_is_refused() {
        let mut capsule = fixture(status(500));
        capsule.job = Some(crate::capsule::schema::CapsuleJob {
            name: "charge".to_owned(),
            payload: serde_json::json!({"api_key": "[FILTERED]", "order": 7}),
        });
        // A job capsule whose payload survived redaction intact still replays.
        assert!(refusal_reason(&capsule).is_none());

        capsule.request.redacted_keys = vec!["job_entry.api_key".to_owned()];
        let reason = refusal_reason(&capsule).expect("a masked job payload is refused");
        assert!(
            reason.contains("api_key"),
            "the refusal must name the field that was masked: {reason}"
        );
        assert!(
            reason.contains("filter_parameters"),
            "and point at the knob that masked it: {reason}"
        );

        // The same key on a *request* capsule is not this refusal: a request
        // body is compared, not parsed, so redaction is tolerated there.
        capsule.job = None;
        assert!(refusal_reason(&capsule).is_none());
    }

    /// A compared field can read `[FILTERED]` as a wildcard. Data the handler
    /// *consumes* cannot: it is deserialized and branched on, so the
    /// placeholder reaches the code as a literal value production never
    /// returned.
    #[test]
    fn a_capsule_whose_replayed_input_was_masked_is_refused() {
        let mut capsule = fixture(status(500));
        assert!(refusal_reason(&capsule).is_none());

        capsule.request.redacted_keys = vec!["http[0].response_body.access_token".to_owned()];
        let reason = refusal_reason(&capsule).expect("a masked response body is refused");
        assert!(
            reason.contains("response_body") && reason.contains("filter_parameters"),
            "the refusal must name the field and the knob: {reason}"
        );

        for key in [
            "cache[0].user_id",
            "http[0].response_header:set-cookie",
            "http[0].final_url.access_token",
            "tenant.id",
        ] {
            capsule.request.redacted_keys = vec![key.to_owned()];
            assert!(
                refusal_reason(&capsule).is_some(),
                "{key} is served to the code as concrete input, so it cannot replay"
            );
        }

        // A masked *request* header is compared, not consumed, so it still
        // replays: the placeholder stands for whatever was really there.
        capsule.request.redacted_keys = vec!["header:authorization".to_owned()];
        assert!(refusal_reason(&capsule).is_none());
        capsule.request.redacted_keys = vec!["http[0].request_header.authorization".to_owned()];
        assert!(refusal_reason(&capsule).is_none());
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

    fn tape(id: u64, sqls: &[&str]) -> ConnectionTape {
        ConnectionTape {
            role: crate::capsule::schema::TAPE_ROLE_PRIMARY.to_owned(),
            id,
            prologue: Vec::new(),
            statements: Vec::new(),
            catalog: Vec::new(),
            exchanges: sqls
                .iter()
                .map(|sql| crate::capsule::schema::Exchange {
                    protocol: crate::capsule::schema::ExchangeProtocol::Extended,
                    sql: (*sql).to_owned(),
                    binds: Vec::new(),
                    response: Vec::new(),
                    row_count: 0,
                    error: None,
                })
                .collect(),
        }
    }

    #[test]
    fn a_fully_consumed_tape_leaves_no_divergence() {
        let log = DivergenceLog::new();
        let progress = log.register_tape(&tape(1, &["SELECT 1", "SELECT 2"]));
        progress.advance();
        progress.advance();
        assert_eq!(progress.unconsumed(), 0);
        assert!(log.unconsumed().is_empty());
    }

    #[test]
    fn leftover_exchanges_name_the_connection_count_and_first_statement() {
        let log = DivergenceLog::new();
        let progress = log.register_tape(&tape(4, &["SELECT 1", "SELECT 2", "SELECT 3"]));
        progress.advance();

        let divergences = log.unconsumed();
        let [divergence] = divergences.as_slice() else {
            panic!("expected exactly one divergence, got {divergences:?}");
        };
        assert_eq!(divergence.kind, DivergenceKind::UnconsumedExchanges);
        assert_eq!(divergence.connection, 4);
        assert_eq!(divergence.exchange_index, 1);
        assert_eq!(divergence.expected_sql.as_deref(), Some("SELECT 2"));
        assert!(
            divergence.detail.contains('2') && divergence.detail.contains("SELECT 2"),
            "the detail must give the count and the first unissued statement, got {:?}",
            divergence.detail
        );
    }

    #[test]
    fn a_tape_no_connection_ever_claimed_is_wholly_unconsumed() {
        let log = DivergenceLog::new();
        let _progress = log.register_tape(&tape(9, &["SELECT 1"]));
        let divergences = log.unconsumed();
        assert_eq!(divergences.len(), 1);
        assert_eq!(
            divergences.first().map(|entry| entry.exchange_index),
            Some(0),
            "nothing was consumed, so the report starts at the first exchange"
        );
    }

    #[test]
    fn an_empty_tape_is_never_a_divergence() {
        let log = DivergenceLog::new();
        let _progress = log.register_tape(&tape(2, &[]));
        assert!(log.unconsumed().is_empty());
    }

    #[tokio::test]
    async fn a_capsule_without_a_database_reproduces_unaffected() {
        let router = axum::Router::new().route("/orders", axum::routing::get(|| async { "ok" }));
        // The recorded outcome is the one this router produces, message and
        // all: a reproduction means the same failure, not merely the same
        // status code.
        let mut capsule = fixture(CapsuleOutcome::Status {
            code: 200,
            message: "OK".to_owned(),
            problem_type: None,
        });
        capsule.db = None;
        let outcome = execute(
            router,
            &capsule,
            Arc::new(DivergenceLog::new()),
            &ReplayFixtures::from_capsule(&capsule),
        )
        .await;
        assert_eq!(outcome.verdict, Verdict::Reproduced, "{outcome:?}");
        assert!(outcome.divergences.is_empty(), "{outcome:?}");
    }

    #[tokio::test]
    async fn unconsumed_exchanges_turn_a_matching_outcome_into_a_divergence() {
        let router = axum::Router::new().route("/orders", axum::routing::get(|| async { "ok" }));
        let capsule = fixture(status(200));
        let log = Arc::new(DivergenceLog::new());
        // Registered but never served: the run reaches the recorded outcome
        // without following the recorded effects.
        let _progress = log.register_tape(&tape(1, &["SELECT 1"]));

        let outcome = execute(
            router,
            &capsule,
            Arc::clone(&log),
            &ReplayFixtures::from_capsule(&capsule),
        )
        .await;
        assert_eq!(outcome.verdict, Verdict::Diverged, "{outcome:?}");
        assert_eq!(
            outcome.divergences.first().map(|entry| entry.kind),
            Some(DivergenceKind::UnconsumedExchanges),
            "{outcome:?}"
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
        let outcome = execute(
            router,
            &capsule,
            Arc::new(DivergenceLog::new()),
            &ReplayFixtures::from_capsule(&capsule),
        )
        .await;
        assert_eq!(outcome.verdict, Verdict::Reproduced, "{outcome:?}");
    }
}
