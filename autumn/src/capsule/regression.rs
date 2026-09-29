//! Turning a capsule into a committed, deterministic regression test (#1634).
//!
//! `autumn replay` answers "is the bug still there?" once. This module answers
//! "can it ever come back?": a capsule the team triaged is copied into the
//! app's test tree and replayed by an ordinary `#[tokio::test]`, so the failure
//! is re-checked on every `cargo test` from then on.
//!
//! The generated test drives **the same** [`execute`](crate::capsule::execute)
//! engine `autumn replay` does, rather than re-deriving the comparison in
//! generated code. That is the whole design: a generated test and the CLI can
//! never disagree about what a reproduction is, and a schema change lands in
//! one place instead of rotting in every committed test.
//!
//! # Zero live dependencies
//!
//! Everything the replayed handler touches comes from the capsule — the clock,
//! the entropy source, outbound HTTP, jobs, cache, mail and the tenant from
//! [`ReplayFixtures`], and the database from the in-process stub server
//! [`pool_from_capsule`](crate::capsule::pool_from_capsule) builds out of the
//! recorded wire tape. No network, no database, no queue, no Docker.
//!
//! # Example
//!
//! ```rust,ignore
//! use autumn_web::capsule::regression::{RegressionCase, RegressionContext};
//! use autumn_web::prelude::*;
//! use autumn_web::test::TestApp;
//!
//! const CAPSULE: &str = include_str!("../capsules/checkout_500.json");
//!
//! fn router(ctx: &RegressionContext<'_>) -> axum::Router {
//!     TestApp::new()
//!         .routes(routes![checkout])
//!         .with_clock(ctx.clock())
//!         .with_entropy(ctx.entropy())
//!         .build()
//!         .into_router()
//! }
//!
//! #[tokio::test]
//! async fn checkout_500_still_reproduces() {
//!     RegressionCase::from_json(CAPSULE)
//!         .expect("the committed capsule parses")
//!         .assert_reproduces(router)
//!         .await;
//! }
//! ```

// Replay-time module (offline replays and generated regression tests, never
// the serving path).
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing,)
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::capsule::replay::{DivergenceLog, ReplayFixtures, ReplayOutcome, Verdict};
use crate::capsule::schema::{Capsule, CapsuleError};
#[cfg(test)]
use futures::FutureExt as _;

/// The default directory a generated corpus lives in, relative to the crate
/// root: `tests/capsules`.
pub const CORPUS_DIR: &str = "tests/capsules";

/// One committed capsule, ready to replay.
#[derive(Debug, Clone)]
pub struct RegressionCase {
    capsule: Capsule,
}

/// What a regression test's router factory is handed.
///
/// Everything here is derived from the one capsule under test, which is what
/// makes several cases safe to run concurrently in a single `cargo test`
/// process: no process-global state is installed, and the effect tape is a
/// task-local scoped to the replayed request.
pub struct RegressionContext<'a> {
    capsule: &'a Capsule,
    fixtures: &'a ReplayFixtures,
    // Read only by `db_pool`, which is feature-gated; a `--no-default-features`
    // build has no reader for it.
    #[cfg_attr(
        not(all(feature = "db", not(feature = "sqlite"))),
        allow(dead_code, reason = "only `db_pool` reads it, and that is gated")
    )]
    divergences: &'a Arc<DivergenceLog>,
}

impl RegressionContext<'_> {
    /// The capsule under test.
    #[must_use]
    pub const fn capsule(&self) -> &Capsule {
        self.capsule
    }

    /// The clock to hand to `TestApp::with_clock`.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn crate::time::ClockSource> {
        self.fixtures.clock()
    }

    /// The entropy source to hand to `TestApp::with_entropy`.
    #[must_use]
    pub fn entropy(&self) -> Arc<dyn crate::entropy::Entropy> {
        self.fixtures.entropy()
    }

    /// A database pool backed by the capsule's recorded wire traffic, for a
    /// capsule whose handler queried a database.
    ///
    /// The pool talks to an **in-process stub server** replaying the recorded
    /// frames, so this needs no database, no container and no network — the
    /// "zero live dependencies" guarantee holds for DB-touching capsules too.
    ///
    /// # Errors
    ///
    /// Returns the pool-construction error when the recorded tape cannot be
    /// rebuilt into a stub server.
    #[cfg(all(feature = "db", not(feature = "sqlite")))]
    pub fn db_pool(
        &self,
    ) -> Result<
        diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>,
        crate::db::PoolError,
    > {
        crate::capsule::pool_from_capsule(self.capsule, Arc::clone(self.divergences))
    }
}

impl RegressionCase {
    /// Load a committed capsule from JSON — typically an `include_str!` of the
    /// fixture `autumn capsule test` wrote.
    ///
    /// # Errors
    ///
    /// Returns [`CapsuleError::Malformed`] for a document that is not a
    /// capsule, and [`CapsuleError::VersionMismatch`] when it was written by an
    /// incompatible format version — the case a committed corpus meets after an
    /// Autumn upgrade, and one that must fail loudly rather than pass
    /// vacuously.
    pub fn from_json(json: &str) -> Result<Self, CapsuleError> {
        Ok(Self {
            capsule: Capsule::from_json(json)?,
        })
    }

    /// Load a committed capsule from a file.
    ///
    /// # Errors
    ///
    /// As [`from_json`](Self::from_json), plus [`CapsuleError::Io`] when the
    /// file cannot be read.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, CapsuleError> {
        let json = std::fs::read_to_string(path.as_ref()).map_err(CapsuleError::Io)?;
        Self::from_json(&json)
    }

    /// The capsule under test.
    #[must_use]
    pub const fn capsule(&self) -> &Capsule {
        &self.capsule
    }

    /// Replay the capsule against the router `build` produces, and return the
    /// verdict.
    ///
    /// Prefer [`assert_reproduces`](Self::assert_reproduces) in a test; this is
    /// for callers that want to inspect the outcome themselves (a whole-corpus
    /// runner that reports every case before failing, say).
    pub async fn run<F>(&self, build: F) -> ReplayOutcome
    where
        F: FnOnce(&RegressionContext<'_>) -> axum::Router,
    {
        let fixtures = ReplayFixtures::from_capsule(&self.capsule);
        let divergences = Arc::new(DivergenceLog::new());
        let router = {
            let context = RegressionContext {
                capsule: &self.capsule,
                fixtures: &fixtures,
                divergences: &divergences,
            };
            build(&context)
        };
        crate::capsule::execute(router, &self.capsule, divergences, &fixtures).await
    }

    /// Replay the capsule and fail the test unless it reproduces.
    ///
    /// # Panics
    ///
    /// Panics — which is how a test fails — when the capsule is one this build
    /// refuses to replay, or when the replayed handler's outcome or effects
    /// diverge from the recording. The panic message is the full report, so a
    /// CI log says *what* changed rather than only that something did.
    pub async fn assert_reproduces<F>(&self, build: F)
    where
        F: FnOnce(&RegressionContext<'_>) -> axum::Router,
    {
        // A refusal is not a verdict: nothing ran, so reporting a failed
        // reproduction would be a lie about what happened.
        if let Some(reason) = crate::capsule::refusal_reason(&self.capsule) {
            panic!("capsule {} cannot be replayed: {reason}", self.capsule.id);
        }
        // A job capsule has no request to drive; putting one through a router
        // would 404 and report a `mismatch`, which the guide tells operators to
        // read as "the bug is gone".
        assert!(
            self.capsule.job.is_none(),
            "capsule {} records a failure inside job {:?}, not a request, so it cannot be \
             replayed against a router — replay it with `autumn replay`, which dispatches the \
             job's handler",
            self.capsule.id,
            self.capsule
                .job
                .as_ref()
                .map(|job| job.name.clone())
                .unwrap_or_default()
        );
        let outcome = self.run(build).await;
        assert!(
            outcome.verdict == Verdict::Reproduced,
            "{}",
            report(&self.capsule, &outcome)
        );
    }

    /// Every capsule file in a corpus directory, in a stable order.
    ///
    /// The order is the file name's, which the capsule writer makes
    /// chronological — so a whole-corpus run reports oldest failure first, the
    /// same way the capsule directory lists.
    ///
    /// # Errors
    ///
    /// Returns the directory-read error, so a missing corpus is reported as
    /// itself rather than as an empty (vacuously passing) run.
    pub fn corpus(dir: impl AsRef<Path>) -> Result<Vec<PathBuf>, std::io::Error> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir.as_ref())?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        Ok(paths)
    }
}

/// The human-readable failure report a diverged regression test prints.
#[must_use]
pub fn report(capsule: &Capsule, outcome: &ReplayOutcome) -> String {
    use std::fmt::Write as _;

    let mut text = format!(
        "capsule {} did not reproduce: verdict `{}`.\n\
         The recorded failure was:\n  {:?}\nThe replayed run produced:\n  {:?}\n",
        capsule.id,
        outcome.verdict.label(),
        outcome.expected,
        outcome.actual,
    );
    if !outcome.divergences.is_empty() {
        let _ = writeln!(text, "\nDatabase divergences:");
        for divergence in &outcome.divergences {
            let _ = writeln!(
                text,
                "  [{}] {}",
                divergence.kind.label(),
                divergence.detail
            );
        }
    }
    if !outcome.effect_divergences.is_empty() {
        let _ = writeln!(text, "\nEffect divergences:");
        for divergence in &outcome.effect_divergences {
            let _ = writeln!(
                text,
                "  [{} / {}] {}",
                divergence.seam.label(),
                divergence.kind.label(),
                divergence.detail
            );
        }
    }
    if !outcome.warnings.is_empty() {
        let _ = writeln!(text, "\nWarnings:");
        for warning in &outcome.warnings {
            let _ = writeln!(text, "  {warning}");
        }
    }
    let _ = writeln!(
        text,
        "\nA `mismatch` usually means the bug is fixed and this test should be \
         deleted (or re-recorded); a `diverged` means the handler's effects changed \
         underneath the capsule."
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capsule::schema::{CapsuleEffects, CapsuleOutcome, HttpEffect};

    fn capsule_json(message: &str) -> String {
        serde_json::json!({
            "format_version": crate::capsule::CAPSULE_FORMAT_VERSION,
            "id": "fixture",
            "captured_at": "2026-08-27T10:00:00Z",
            "autumn_version": env!("CARGO_PKG_VERSION"),
            "request": {
                "method": "GET",
                "uri": "/boom",
                "http_version": "HTTP/1.1",
                "headers": [],
                "body": "absent",
            },
            "outcome": {"status": {"code": 500, "message": message}},
        })
        .to_string()
    }

    /// A router that answers `/boom` with the given status and message.
    fn router(status: u16, message: &'static str) -> axum::Router {
        use axum::routing::get;
        axum::Router::new().route(
            "/boom",
            get(move || async move {
                (
                    axum::http::StatusCode::from_u16(status)
                        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
                    message,
                )
            }),
        )
    }

    #[tokio::test]
    async fn a_regression_case_reproduces_when_the_outcome_still_matches() {
        let case = RegressionCase::from_json(&capsule_json("Internal Server Error"))
            .expect("the fixture parses");
        let outcome = case.run(|_| router(500, "still broken")).await;
        assert_eq!(outcome.verdict, Verdict::Reproduced, "{outcome:?}");
    }

    /// The operative half of the acceptance criterion: a generated test that
    /// can only ever pass is not a regression test.
    #[tokio::test]
    async fn a_regression_case_fails_the_test_when_the_outcome_diverges() {
        let case = RegressionCase::from_json(&capsule_json("Internal Server Error"))
            .expect("the fixture parses");
        let outcome = case.run(|_| router(200, "fixed")).await;
        assert_eq!(outcome.verdict, Verdict::Mismatch, "{outcome:?}");

        let panicked =
            std::panic::AssertUnwindSafe(case.assert_reproduces(|_| router(200, "fixed")))
                .catch_unwind()
                .await;
        let payload = panicked.expect_err("a mismatch must fail the test");
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(
            message.contains("did not reproduce") && message.contains("mismatch"),
            "the failure must say what changed: {message}"
        );
    }

    /// An effect the replayed code performs that the recording never did fails
    /// the test too, not only a changed status.
    #[tokio::test]
    async fn a_regression_case_fails_on_an_unconsumed_effect() {
        let mut capsule =
            Capsule::from_json(&capsule_json("Internal Server Error")).expect("parses");
        capsule.effects = CapsuleEffects {
            http: vec![HttpEffect {
                method: "GET".to_owned(),
                url: "https://api.example/thing".to_owned(),
                request_headers: Vec::new(),
                request_body: crate::capsule::CapsuleBody::Absent,
                status: 200,
                response_headers: Vec::new(),
                response_body: crate::capsule::CapsuleBody::Absent,
                error: None,
                ..Default::default()
            }],
            ..CapsuleEffects::default()
        };
        let case = RegressionCase {
            capsule: capsule.clone(),
        };
        // The router never makes the recorded call.
        let outcome = case.run(|_| router(500, "still broken")).await;
        assert_eq!(outcome.verdict, Verdict::Diverged, "{outcome:?}");
        assert!(
            outcome
                .effect_divergences
                .iter()
                .any(|divergence| divergence.detail.contains("never asked for")),
            "{:?}",
            outcome.effect_divergences
        );
    }

    #[tokio::test]
    async fn a_job_capsule_is_refused_rather_than_driven_through_a_router() {
        let mut value: serde_json::Value =
            serde_json::from_str(&capsule_json("boom")).expect("parses");
        value["job"] = serde_json::json!({"name": "send_receipt", "payload": {}});
        let case = RegressionCase::from_json(&value.to_string()).expect("parses");
        let panicked = std::panic::AssertUnwindSafe(case.assert_reproduces(|_| router(500, "x")))
            .catch_unwind()
            .await;
        assert!(
            panicked.is_err(),
            "a job capsule must not be router-replayed"
        );
    }

    #[test]
    fn a_corpus_lists_capsules_in_a_stable_order_and_reports_a_missing_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["b.json", "a.json", "notes.txt"] {
            std::fs::write(dir.path().join(name), "{}").expect("write");
        }
        let paths = RegressionCase::corpus(dir.path()).expect("the corpus lists");
        let names: Vec<String> = paths
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.json".to_owned(), "b.json".to_owned()]);

        RegressionCase::corpus(dir.path().join("missing"))
            .expect_err("a missing corpus is reported, not silently empty");
    }

    #[test]
    fn the_failure_report_names_the_verdict_and_both_outcomes() {
        let capsule = Capsule::from_json(&capsule_json("boom")).expect("parses");
        let outcome = ReplayOutcome {
            verdict: Verdict::Mismatch,
            expected: capsule.outcome.clone(),
            actual: CapsuleOutcome::Status {
                code: 200,
                message: "fixed".to_owned(),
                problem_type: None,
            },
            divergences: Vec::new(),
            effect_divergences: Vec::new(),
            warnings: vec!["a warning".to_owned()],
        };
        let text = report(&capsule, &outcome);
        assert!(text.contains("mismatch"), "{text}");
        assert!(text.contains("fixed"), "{text}");
        assert!(text.contains("a warning"), "{text}");
    }
}
