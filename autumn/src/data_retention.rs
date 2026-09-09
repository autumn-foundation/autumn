//! Unified data-retention policy for framework-owned data (issue #1605).
//!
//! Autumn creates and fills persistent stores the application never asked
//! for — the job queue and its tracking records, idempotency replay records,
//! sticky experiment assignments, webhook replay markers, sessions, audit
//! archives. Being batteries-included means owning their lifecycle, so this
//! module is the one place that:
//!
//! 1. enumerates every framework-owned dataset ([`RETENTION_DATASETS`]),
//! 2. resolves the window each one is actually kept for
//!    ([`effective_retention`]), reconciling the `[retention]` policy with
//!    the pre-existing per-subsystem TTL knobs,
//! 3. refuses to touch anything under a GDPR legal hold ([`legal_hold_for`]),
//! 4. runs the sweep ([`run_retention`]) — on a recurring in-process schedule
//!    ([`framework_retention_task`]) and on demand from
//!    `autumn db retention`, through the *same* code path so the report and
//!    the enforcement can never disagree, and
//! 5. writes an audit record for every run.
//!
//! Not to be confused with [`crate::retention`], which sweeps **app-defined**
//! models declared with `#[repository(..., retention(...))]` (issue #1342).
//! That module owns your tables; this one owns Autumn's.
//!
//! See `docs/guide/data-retention.md`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::AutumnResult;
use crate::config::{AutumnConfig, RetentionConfig};
use crate::gdpr::{ErasureStrategy, GdprRegistry};
use crate::state::AppState;
use crate::task::{Schedule, TaskCoordination, TaskInfo};

/// The scheduled-task name the framework sweep registers under.
///
/// Shares one namespace with every `#[scheduled(name = "...")]` fn and with
/// `#[repository(..., retention(...))]`'s generated `retention-sweep-<table>`
/// names, so it is deliberately prefixed to stay out of the way.
pub const RETENTION_SWEEP_TASK_NAME: &str = "autumn-retention-sweep";

/// The audit action recorded for every sweep of every dataset.
pub const RETENTION_AUDIT_ACTION: &str = "retention.sweep";

/// The actor recorded on retention audit events — the framework itself,
/// never a user.
pub const RETENTION_AUDIT_ACTOR: &str = "autumn:retention";

/// How many rows one `DELETE` removes. A sweep never issues one unbounded
/// delete: it walks the stale rows in batches so a single run cannot hold a
/// long lock or spike replication lag on a hot table.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
const SWEEP_BATCH_ROWS: i64 = 500;

/// Upper bound on batches per dataset per run. A run that hits this stops and
/// resumes on the next tick, so a first sweep of a years-old table is spread
/// over several ticks instead of one very long transaction storm.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
const SWEEP_MAX_BATCHES: usize = 1_000;

// ── Dataset registry ─────────────────────────────────────────────────────

/// How a dataset's retention window is actually enforced.
///
/// The datasets are genuinely heterogeneous — three Postgres tables, three
/// TTL-native stores, one file archive — and pretending otherwise would mean
/// a "sweep" that always reports zero for half of them. Reporting the
/// mechanism per dataset makes that legible instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionEnforcement {
    /// A scheduled batched `DELETE` against a framework-owned Postgres table.
    Sweep,
    /// The storage backend expires the record itself; the retention window is
    /// enforced by *capping* the TTL the record is written with, so it can
    /// only ever shorten the lifetime, never extend it.
    BackendTtl,
    /// The audit archive is rewritten in place without the stale entries.
    ArchiveRewrite,
}

impl std::fmt::Display for RetentionEnforcement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Sweep => "sweep",
            Self::BackendTtl => "backend ttl",
            Self::ArchiveRewrite => "archive rewrite",
        })
    }
}

/// One framework-owned persistent dataset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionDataset {
    /// Terminal (`completed`/`failed`/`discarded`) rows in `autumn_jobs`.
    JobHistory,
    /// Terminal rows in the `#[after_commit]` hook queue,
    /// `autumn_repository_commit_hooks`.
    CommitHooks,
    /// Progress/result records in `autumn_job_tracking`.
    JobTracking,
    /// Stored idempotency-key responses.
    Idempotency,
    /// Sticky rows in `autumn_experiment_assignments`.
    ExperimentAssignments,
    /// Inbound webhook replay markers.
    WebhookReplay,
    /// Server-side session records.
    Sessions,
    /// Entries in the JSONL audit archive.
    AuditArchives,
}

/// Every framework-owned dataset, in the order the CLI reports them.
///
/// The single list the config surface, the sweeps, the CLI report and the
/// docs table all derive from.
pub const RETENTION_DATASETS: [RetentionDataset; 8] = [
    RetentionDataset::JobHistory,
    RetentionDataset::CommitHooks,
    RetentionDataset::JobTracking,
    RetentionDataset::Idempotency,
    RetentionDataset::ExperimentAssignments,
    RetentionDataset::WebhookReplay,
    RetentionDataset::Sessions,
    RetentionDataset::AuditArchives,
];

impl RetentionDataset {
    /// The `[retention]` config key and CLI `--dataset` value.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::JobHistory => "job_history",
            Self::CommitHooks => "commit_hooks",
            Self::JobTracking => "job_tracking",
            Self::Idempotency => "idempotency",
            Self::ExperimentAssignments => "experiment_assignments",
            Self::WebhookReplay => "webhook_replay",
            Self::Sessions => "sessions",
            Self::AuditArchives => "audit_archives",
        }
    }

    /// Resolve a dataset from its config key. `None` for anything unknown, so
    /// a mistyped `--dataset` errors rather than silently sweeping nothing.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        RETENTION_DATASETS
            .into_iter()
            .find(|dataset| dataset.key() == key)
    }

    /// One-line description shown in the CLI report and the docs table.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::JobHistory => "Finished job rows (completed/failed/discarded) in autumn_jobs",
            Self::CommitHooks => {
                "Finished #[after_commit] hook rows in autumn_repository_commit_hooks"
            }
            Self::JobTracking => "Tracked-job progress/result records in autumn_job_tracking",
            Self::Idempotency => "Stored Idempotency-Key responses",
            Self::ExperimentAssignments => {
                "Sticky experiment assignments in autumn_experiment_assignments"
            }
            Self::WebhookReplay => "Inbound webhook replay markers",
            Self::Sessions => "Server-side session records",
            Self::AuditArchives => "Entries in the JSONL audit archive",
        }
    }

    /// The Postgres table this dataset lives in, when it lives in one.
    ///
    /// Also the key a GDPR legal hold is registered against — see
    /// [`legal_hold_for`].
    #[must_use]
    pub const fn table(self) -> Option<&'static str> {
        match self {
            Self::JobHistory => Some("autumn_jobs"),
            Self::CommitHooks => Some("autumn_repository_commit_hooks"),
            Self::JobTracking => Some("autumn_job_tracking"),
            Self::ExperimentAssignments => Some("autumn_experiment_assignments"),
            Self::Idempotency | Self::WebhookReplay | Self::Sessions | Self::AuditArchives => None,
        }
    }

    /// How this dataset's window is enforced *for a given configuration*.
    ///
    /// Only `job_tracking` varies: tracked-job records live wherever
    /// `jobs.backend` puts them. Under `postgres` they are rows in
    /// `autumn_job_tracking` and a sweep deletes them (which is also what
    /// lets a GDPR legal hold stop the deletion). Under `redis` — or the
    /// in-memory fallback — there is no table to sweep and the record's own
    /// TTL is the only bound, so the window is applied by capping that TTL at
    /// write time instead. Reporting `sweep` for a Redis deployment would
    /// claim a policy nothing enforces.
    #[must_use]
    pub fn enforcement_for(self, config: &AutumnConfig) -> RetentionEnforcement {
        if self == Self::JobTracking && config.jobs.backend != "postgres" {
            return RetentionEnforcement::BackendTtl;
        }
        self.enforcement()
    }

    /// How this dataset's window is enforced in the common case.
    ///
    /// Configuration-independent; see [`Self::enforcement_for`], which is
    /// what the engine and the report actually use.
    #[must_use]
    pub const fn enforcement(self) -> RetentionEnforcement {
        match self {
            Self::JobHistory
            | Self::CommitHooks
            | Self::JobTracking
            | Self::ExperimentAssignments => RetentionEnforcement::Sweep,
            Self::Idempotency | Self::WebhookReplay | Self::Sessions => {
                RetentionEnforcement::BackendTtl
            }
            Self::AuditArchives => RetentionEnforcement::ArchiveRewrite,
        }
    }

    /// What happens with no `[retention]` window set — the documented
    /// default, "forever" where applicable (AC #7).
    #[must_use]
    pub const fn default_behavior(self) -> &'static str {
        match self {
            Self::JobHistory
            | Self::CommitHooks
            | Self::ExperimentAssignments
            | Self::AuditArchives => "forever",
            Self::JobTracking => "jobs.tracking.ttl_secs (24h by default)",
            Self::Idempotency => "idempotency.ttl_secs (24h by default)",
            Self::WebhookReplay => "the endpoint's replay_window_secs (24h by default)",
            Self::Sessions => "session.max_age_secs (the session cookie's lifetime)",
        }
    }

    /// The pre-existing per-subsystem knob that already bounds this dataset,
    /// as `(config key, window)` — the other half of the precedence rule in
    /// [`effective_retention`].
    fn subsystem_ttl(self, config: &AutumnConfig) -> Option<(&'static str, Duration)> {
        match self {
            Self::JobTracking => Some((
                "jobs.tracking.ttl_secs",
                Duration::from_secs(config.jobs.tracking.ttl_secs),
            )),
            Self::Idempotency => Some((
                "idempotency.ttl_secs",
                Duration::from_secs(config.idempotency.ttl_secs),
            )),
            Self::Sessions => Some((
                "session.max_age_secs",
                Duration::from_secs(config.session.max_age_secs),
            )),
            Self::WebhookReplay => {
                // Replay windows are declared per endpoint; the longest one
                // is the bound that actually governs how long any marker can
                // survive, so that is what the unified window competes with.
                // Only endpoints that actually write markers count, matching
                // `AutumnConfig::validate_retention_against_replay_protection`
                // — an endpoint with replay protection off stores nothing, so
                // its window must not set this dataset's reported bound.
                let longest = config
                    .security
                    .webhooks
                    .endpoints
                    .iter()
                    .filter(|endpoint| endpoint.replay_protection)
                    .map(|endpoint| endpoint.replay_window_secs)
                    .max();
                longest.map(|secs| {
                    (
                        "security.webhooks.endpoints[].replay_window_secs",
                        Duration::from_secs(secs),
                    )
                })
            }
            Self::JobHistory
            | Self::CommitHooks
            | Self::ExperimentAssignments
            | Self::AuditArchives => None,
        }
    }
}

impl std::fmt::Display for RetentionDataset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.key())
    }
}

// ── Effective window ─────────────────────────────────────────────────────

/// Where a dataset's effective retention window came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionSource {
    /// Nothing bounds this dataset: it is kept forever.
    Unset,
    /// The `[retention]` policy window is what actually applies.
    ///
    /// Named `RetentionSection` rather than `Policy` so the crate exports no
    /// second public item called `Policy`: a duplicate name stops rustc from
    /// trimming paths, which turns `Policy<Widget>` into
    /// `autumn_web::authorization::Policy<Widget>` in every authorization
    /// diagnostic users see.
    RetentionSection,
    /// A pre-existing per-subsystem knob is what actually applies, named
    /// here by its config key.
    SubsystemTtl(&'static str),
}

impl std::fmt::Display for RetentionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unset => f.write_str("unset"),
            Self::RetentionSection => f.write_str("[retention]"),
            Self::SubsystemTtl(key) => write!(f, "{key}"),
        }
    }
}

/// A dataset's resolved retention window and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveRetention {
    /// The dataset this describes.
    pub dataset: RetentionDataset,
    /// How long records are kept. `None` means forever.
    pub window: Option<Duration>,
    /// Which setting produced [`Self::window`].
    pub source: RetentionSource,
}

/// Resolve how long `dataset` is actually kept for.
///
/// **Precedence (AC #3): the shorter bound wins.** The pre-existing
/// per-subsystem knobs (`jobs.tracking.ttl_secs`, `idempotency.ttl_secs`,
/// `session.max_age_secs`, a webhook endpoint's `replay_window_secs`) keep
/// their exact meaning and keep working with no `[retention]` section at all.
/// A `[retention]` window is an *additional, independent ceiling*: with both
/// set, records live for `min(subsystem ttl, policy window)`.
///
/// That rule is chosen so the unified section can only ever tighten
/// retention, never silently extend a bound the operator already set
/// elsewhere — there is no configuration in which adding `[retention]` causes
/// data to be kept *longer* than it is today.
#[must_use]
pub fn effective_retention(config: &AutumnConfig, dataset: RetentionDataset) -> EffectiveRetention {
    let policy = config.retention.window(dataset.key());
    let subsystem = dataset.subsystem_ttl(config);
    let (window, source) = match (policy, subsystem) {
        (None, None) => (None, RetentionSource::Unset),
        (Some(policy), None) => (Some(policy), RetentionSource::RetentionSection),
        (None, Some((key, ttl))) => (Some(ttl), RetentionSource::SubsystemTtl(key)),
        (Some(policy), Some((key, ttl))) => {
            if policy <= ttl {
                (Some(policy), RetentionSource::RetentionSection)
            } else {
                (Some(ttl), RetentionSource::SubsystemTtl(key))
            }
        }
    };
    EffectiveRetention {
        dataset,
        window,
        source,
    }
}

// ── Legal hold ───────────────────────────────────────────────────────────

/// The legal-hold reason blocking `dataset` from ever being swept, if any
/// (AC #5).
///
/// A dataset is on hold when its backing table is registered in the
/// application's [`GdprRegistry`] with [`ErasureStrategy::Retain`] — the same
/// registration that already exempts a table from a GDPR erasure request.
/// Holding data is a legal obligation that outranks a retention window, so
/// this is a **veto over the whole dataset**, not a row filter: a sweep that
/// cannot tell held rows from unheld ones must not delete any of them.
///
/// Datasets with no backing table (`idempotency`, `webhook_replay`,
/// `sessions`, `audit_archives`) can never be placed on hold this way, since
/// a GDPR registration names a table.
#[must_use]
pub fn legal_hold_for(
    dataset: RetentionDataset,
    registry: Option<&GdprRegistry>,
) -> Option<String> {
    let table = dataset.table()?;
    let registration = registry?.get(table)?;
    if registration.erasure_strategy != ErasureStrategy::Retain {
        return None;
    }
    Some(
        registration
            .retain_reason
            .clone()
            .unwrap_or_else(|| format!("{table} is registered under GDPR legal hold")),
    )
}

// ── Report ───────────────────────────────────────────────────────────────

/// What one dataset's retention looks like, and what a run did to it.
///
/// The same struct backs the scheduler's log line, the audit record, and the
/// `autumn db retention` report, so those three can never disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionDatasetReport {
    /// The dataset's config key (`job_history`, …).
    pub dataset: String,
    /// One-line human-readable description.
    pub description: String,
    /// How the window is enforced (`sweep` / `backend ttl` / `archive rewrite`).
    pub enforcement: String,
    /// The effective window in seconds. `None` means "kept forever".
    pub window_secs: Option<u64>,
    /// Which setting produced the window (`[retention]`, a subsystem key, or
    /// `unset`).
    pub source: String,
    /// The instant records older than which are eligible, RFC-3339. `None`
    /// when nothing bounds the dataset.
    pub cutoff: Option<String>,
    /// Rows/entries currently older than the cutoff. `None` when the dataset
    /// cannot be counted from here (a TTL-native store the backend expires
    /// on its own).
    pub eligible_rows: Option<u64>,
    /// Rows/entries actually removed by this run (always `0` for a dry run).
    pub rows_removed: u64,
    /// `true` when the run stopped at its per-run batch cap with rows still
    /// stale — the policy was only *partially* enforced this tick and resumes
    /// on the next one.
    ///
    /// Reported (and audited) rather than left implicit: a truncated run that
    /// looked clean in the audit trail would let a reviewer conclude a policy
    /// was enforced when millions of rows past the window were still there.
    #[serde(default)]
    pub truncated: bool,
    /// `true` when nothing was deleted.
    pub dry_run: bool,
    /// Why this dataset was not swept, when it was not: a legal hold, a
    /// missing database, or a backend that expires records itself.
    pub skipped: Option<String>,
    /// Wall-clock time this dataset's pass took, in milliseconds.
    pub duration_ms: u64,
    /// The error this dataset's pass failed with, if it did. One dataset
    /// failing never aborts the others.
    pub error: Option<String>,
}

/// Options for one [`run_retention`] pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct RetentionRunOptions<'a> {
    /// Count what would be removed and delete nothing.
    pub dry_run: bool,
    /// Restrict the run to one dataset key. `None` runs every dataset.
    pub dataset: Option<&'a str>,
}

// ── Engine ───────────────────────────────────────────────────────────────

/// Run the retention policy over every configured dataset (or the one named
/// by [`RetentionRunOptions::dataset`]).
///
/// This is the single engine behind both the recurring in-process sweep and
/// `autumn db retention` — the CLI never re-implements the policy, so the
/// numbers it reports are the numbers the app enforces.
///
/// A dataset that errors is reported in its own
/// [`RetentionDatasetReport::error`] and never aborts the rest of the run: a
/// transient pool timeout on one table must not stop another table from being
/// bounded.
///
/// Every dataset that was actually eligible to be swept emits an audit record
/// carrying the dataset, the cutoff, and the number of rows removed (AC #6).
///
/// # Errors
///
/// Returns an error only when [`RetentionRunOptions::dataset`] names a
/// dataset that does not exist — a mistyped `--dataset` must fail loudly
/// rather than silently sweep nothing.
pub async fn run_retention(
    state: &AppState,
    options: &RetentionRunOptions<'_>,
) -> AutumnResult<Vec<RetentionDatasetReport>> {
    let datasets = resolve_datasets(options.dataset)?;
    let config = state.config_arc();
    let mut reports = Vec::with_capacity(datasets.len());
    for dataset in datasets {
        let report = run_one_dataset(state, &config, dataset, options.dry_run).await;
        audit_sweep(state, &report).await;
        reports.push(report);
    }
    Ok(reports)
}

/// Resolve a `--dataset` filter to the datasets a run should cover.
///
/// # Errors
///
/// Returns a not-found error naming the valid keys when `filter` matches no
/// registered dataset.
fn resolve_datasets(filter: Option<&str>) -> AutumnResult<Vec<RetentionDataset>> {
    let Some(filter) = filter else {
        return Ok(RETENTION_DATASETS.to_vec());
    };
    RetentionDataset::from_key(filter).map_or_else(
        || {
            let known: Vec<&str> = RETENTION_DATASETS.iter().map(|d| d.key()).collect();
            Err(crate::AutumnError::not_found_msg(format!(
                "unknown retention dataset {filter:?}; known datasets: {}",
                known.join(", ")
            )))
        },
        |dataset| Ok(vec![dataset]),
    )
}

/// Run (or dry-run) one dataset's pass, capturing every outcome into a
/// report rather than propagating it.
async fn run_one_dataset(
    state: &AppState,
    config: &AutumnConfig,
    dataset: RetentionDataset,
    dry_run: bool,
) -> RetentionDatasetReport {
    let started = std::time::Instant::now();
    let effective = effective_retention(config, dataset);
    // A provisional cutoff, so a dataset with no database still reports one.
    // The sweep path replaces it with the instant Postgres itself resolved —
    // see `sweep_postgres`.
    let cutoff = effective
        .window
        .and_then(|window| chrono::Duration::from_std(window).ok())
        .map(|window| Utc::now() - window);

    let mut report = RetentionDatasetReport {
        dataset: dataset.key().to_owned(),
        description: dataset.description().to_owned(),
        enforcement: dataset.enforcement_for(config).to_string(),
        window_secs: effective.window.map(|w| w.as_secs()),
        source: effective.source.to_string(),
        cutoff: cutoff.map(|c| c.to_rfc3339()),
        eligible_rows: None,
        rows_removed: 0,
        truncated: false,
        dry_run,
        skipped: None,
        duration_ms: 0,
        error: None,
    };

    let (Some(window), Some(cutoff)) = (effective.window, cutoff) else {
        report.skipped = Some("no retention window configured".to_owned());
        report.duration_ms = elapsed_ms(started);
        return report;
    };

    // A legal hold outranks the window: check it before doing any counting,
    // so a held dataset never even reads the rows it must not delete.
    let registry = state.extension::<GdprRegistry>();
    if let Some(reason) = legal_hold_for(dataset, registry.as_deref()) {
        report.skipped = Some(format!("legal hold: {reason}"));
        report.duration_ms = elapsed_ms(started);
        return report;
    }

    match dataset.enforcement_for(config) {
        RetentionEnforcement::Sweep => {
            apply_sweep(state, dataset, window, dry_run, &mut report).await;
        }
        RetentionEnforcement::BackendTtl => {
            // Not a sweep: the window is applied when the record is written,
            // by capping its TTL (`AutumnConfig::apply_retention_caps`).
            // There is nothing here to count or delete — and saying so beats
            // reporting a fake zero.
            //
            // The caveat is stated rather than glossed: a write-time cap
            // cannot reach a record that already exists. Records written
            // before the window was shortened keep the TTL they were stored
            // with and age out under it, so enforcement is complete only once
            // the previous TTL has elapsed. Claiming otherwise would make the
            // report assert a bound the data does not yet satisfy.
            report.skipped = Some(backend_ttl_note(
                dataset,
                config,
                effective.window.map_or(0, |window| window.as_secs()),
            ));
        }
        RetentionEnforcement::ArchiveRewrite => {
            apply_archive_purge(state, cutoff, dry_run, &mut report).await;
        }
    }

    report.duration_ms = elapsed_ms(started);
    report
}

/// The report note for a `backend ttl` dataset: what the window means here,
/// and — importantly — where it does *not* hold.
///
/// A write-time cap cannot reach a record that already exists, and the
/// in-memory session store records no expiry at all, so claiming plain
/// "enforced by the backend" would present an unenforced policy as an
/// enforced one. Each caveat is named rather than left to the guide.
fn backend_ttl_note(dataset: RetentionDataset, config: &AutumnConfig, window_secs: u64) -> String {
    // `session.backend = "memory"` is the default, and `MemoryStore` is a
    // plain `HashMap` with no expiry: capping `session.max_age_secs` shortens
    // only the browser cookie while the server-side record lives as long as
    // the process. That is not retention, and the report must not call it
    // that.
    if dataset == RetentionDataset::Sessions
        && config.session.backend == crate::session::SessionBackend::Memory
    {
        return format!(
            "NOT enforced: the in-memory session store keeps records for the life of the \
             process, so this {window_secs}s window bounds only the session cookie. Set \
             session.backend = \"redis\" (or install a SessionStore that applies the \
             window) to bound the server-side records"
        );
    }

    let mut note = format!(
        "enforced at write time: records are stored with a TTL capped at {window_secs}s by \
         the backend, not deleted by this sweep. Records written before this window was set \
         keep their original TTL until it elapses"
    );
    if dataset == RetentionDataset::Sessions {
        note.push_str(
            ", and a custom SessionStore installed with AppBuilder::with_session_store must \
             apply the window itself",
        );
    }
    note
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Purge stale audit-archive entries through the installed [`AuditLogger`].
async fn apply_archive_purge(
    state: &AppState,
    cutoff: DateTime<Utc>,
    dry_run: bool,
    report: &mut RetentionDatasetReport,
) {
    let Some(logger) = state.extension::<crate::audit::AuditLogger>() else {
        report.skipped = Some("no audit logger is installed".to_owned());
        return;
    };
    // One pass, not two: on a real run `entries_removed` *is* what was
    // eligible, and the archive can be large enough that reading it twice per
    // sweep is a cost worth not paying.
    let summary = logger.purge_before(cutoff, dry_run).await;
    // Record what was removed even when a sink failed. A partial purge has
    // already deleted those entries; reporting `rows_removed = 0` alongside
    // the error would understate a deletion that really happened.
    if summary.supported {
        report.eligible_rows = Some(summary.entries_removed);
        if !dry_run {
            report.rows_removed = summary.entries_removed;
        }
    } else if summary.errors.is_empty() {
        report.skipped = Some(
            "no installed audit sink supports purging (see AuditSink::purge_before)".to_owned(),
        );
    }
    report.error = summary.error_message();
}

// ── Postgres sweeps ──────────────────────────────────────────────────────

/// The `WHERE` clause selecting stale rows for a sweep-enforced dataset,
/// with `$1` bound to the cutoff timestamp.
///
/// Every predicate is deliberately conservative about what "stale" means. In
/// each case the extra conditions are not defensive padding — each one guards
/// a specific invariant another subsystem depends on:
///
/// - **`job_history`** matches only rows in a **terminal** state
///   (`completed`/`failed`/`discarded`) with a recorded `finished_at`, so a
///   job that is enqueued, running, or waiting on a retry is untouchable no
///   matter how old its row is (a retry goes back to `status = 'enqueued'`
///   with `finished_at = NULL`, see `job.rs`'s `pg_recover_stale_claims`).
///
///   It additionally never touches a row that still carries a **TTL-window
///   dedup key**. `#[job(unique, unique_for_ms = N)]` enforces its window
///   purely by the continued existence of the historical row — `job.rs`'s
///   `DEDUP_GUARD` matches `dup.enqueued_at > NOW() - N` with *no* status
///   filter, deliberately, so a completed twin still suppresses a duplicate
///   enqueue. Deleting that row would silently run the job twice. The row
///   itself does not record `N` (it is a compile-time `#[job]` attribute, not
///   a column), so there is no cutoff at which the sweep could safely take it;
///   it is retained until the key is cleared. See
///   `docs/guide/data-retention.md`.
///
/// - **`experiment_assignments`** matches only assignments belonging to an
///   `archived` experiment, or one whose experiment row is gone. A sticky
///   assignment is what keeps an actor on one variant while an experiment
///   runs: deleting it re-buckets the actor through the *current* weights,
///   which silently contaminates a running experiment's results and can also
///   admit the actor into a sibling experiment in the same exclusion group.
///
///   The boundary is restartability, not "finished". `ExperimentService::start`
///   restores a `draft` or `concluded` experiment to `running` and refuses only
///   `archived`, so `concluded` assignments are still live data — a sweep
///   followed by a restart re-buckets every returning actor. A deleted
///   experiment row is safe for the same reason in reverse: nothing can restart
///   it, so nothing can read those assignments again.
#[cfg(any(test, all(feature = "db", not(feature = "sqlite"))))]
#[must_use]
const fn stale_row_predicate(dataset: RetentionDataset) -> Option<(&'static str, &'static str)> {
    match dataset {
        RetentionDataset::JobHistory => Some((
            "autumn_jobs",
            "status IN ('completed', 'failed', 'discarded') AND finished_at IS NOT NULL              AND finished_at < $1              AND NOT (unique_key IS NOT NULL AND unique_window = 'ttl')",
        )),
        RetentionDataset::CommitHooks => Some((
            "autumn_repository_commit_hooks",
            // `after_hook_failed` is terminal too: it records `finished_at`
            // and the row is only ever revived by an `ON CONFLICT (id)`
            // upsert, which inserts cleanly if the row is gone. Omitting it
            // would leave every permanently-failed after-hook accumulating
            // forever despite a configured window.
            "status IN ('completed', 'failed', 'after_hook_failed') \
             AND finished_at IS NOT NULL AND finished_at < $1",
        )),
        RetentionDataset::JobTracking => Some(("autumn_job_tracking", "updated_at < $1")),
        RetentionDataset::ExperimentAssignments => Some((
            "autumn_experiment_assignments",
            // Only `archived` is terminal. `ExperimentService::start` restores
            // a `draft` OR `concluded` experiment to `running` and refuses only
            // `archived` — so a concluded experiment's sticky assignments are
            // still live data (#1605 review round 13). Sweeping them and then
            // restarting re-buckets every returning actor through the current
            // weights, which is the precise corruption this predicate was
            // written to avoid; the original set just drew the line at the
            // wrong state.
            //
            // Assignments whose experiment row is gone entirely are still
            // swept: a deleted experiment cannot be restarted, so nothing can
            // re-read them.
            "assigned_at < $1 AND NOT EXISTS (                  SELECT 1 FROM autumn_experiments e                  WHERE e.name = autumn_experiment_assignments.experiment                    AND e.state IN ('draft', 'running', 'concluded')              )",
        )),
        RetentionDataset::Idempotency
        | RetentionDataset::WebhookReplay
        | RetentionDataset::Sessions
        | RetentionDataset::AuditArchives => None,
    }
}

/// The primary-key column each sweepable table batches by.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
const fn sweep_key_column(dataset: RetentionDataset) -> &'static str {
    match dataset {
        RetentionDataset::JobTracking => "key",
        _ => "id",
    }
}

#[cfg(all(feature = "db", not(feature = "sqlite")))]
async fn apply_sweep(
    state: &AppState,
    dataset: RetentionDataset,
    window: Duration,
    dry_run: bool,
    report: &mut RetentionDatasetReport,
) {
    let Some(pool) = state.pool().cloned() else {
        report.skipped =
            Some("no database is configured, so this dataset has no rows here".to_owned());
        return;
    };
    match sweep_postgres(&pool, dataset, window, dry_run).await {
        Ok(swept) => {
            // Overwrite the provisional app-clock cutoff with the instant the
            // database actually applied, so the report and the audit record
            // state the real one.
            report.cutoff = Some(swept.cutoff.to_rfc3339());
            report.eligible_rows = Some(swept.eligible);
            report.rows_removed = swept.removed;
            report.truncated = swept.truncated;
        }
        Err(failure) => {
            // Batches are separate autocommit statements, so whatever earlier
            // ones deleted is permanently gone. Report it alongside the
            // failure rather than auditing a real deletion as zero, and mark
            // the pass truncated — it certainly did not drain the table.
            //
            // The cutoff comes with it once the database resolved one: those
            // deletes used *that* boundary, and leaving the provisional
            // app-clock value in place would have the audit record name a
            // different one under clock skew.
            if let Some(cutoff) = failure.cutoff {
                report.cutoff = Some(cutoff.to_rfc3339());
            }
            report.rows_removed = failure.removed;
            report.truncated = true;
            report.error = Some(failure.error.to_string());
        }
    }
}

/// The sweep itself is Postgres-only. Its predicates and batching are written
/// against Postgres types and `NOW()`, and `autumn_experiment_assignments` is
/// created by a Postgres-specific migration, so there is nothing here to run on
/// `SQLite`. Report that rather than pretending a sweep ran.
///
/// The durable `SQLite` job backend (issue #1907) does create `autumn_jobs` and
/// `autumn_job_tracking` in the app's own file, and prunes them itself from its
/// maintenance loop — expired tracking records always, and terminal job rows
/// when `retention.job_history` is set. A `SQLite` sweep driven from
/// `autumn db retention` is tracked under #1909.
#[cfg(any(not(feature = "db"), feature = "sqlite"))]
#[allow(clippy::unused_async)]
async fn apply_sweep(
    _state: &AppState,
    _dataset: RetentionDataset,
    _window: Duration,
    _dry_run: bool,
    report: &mut RetentionDatasetReport,
) {
    report.skipped = Some(
        if cfg!(feature = "sqlite") {
            "the retention sweep is Postgres-only; on SQLite the durable job runtime prunes \
             its own tables (see retention.job_history) and a sweep for the rest is planned"
        } else {
            "this build has no database support (`db` feature off)"
        }
        .to_owned(),
    );
}

/// Resolve the sweep cutoff **on the database server** and count/delete
/// against it.
///
/// The cutoff is `NOW() - <window>` evaluated by Postgres, not
/// `Utc::now() - window` in the app. Every column these predicates compare
/// against is written with the database's `NOW()` (`finished_at`,
/// `updated_at`, `assigned_at`), so using the app's clock would compare two
/// different clocks: a worker replica running fast by δ would delete rows δ
/// younger than the declared window — over-deleting, the compliance-relevant
/// direction. The resolved instant is returned so the report and the audit
/// record state the cutoff that was actually applied.
///
/// Returns what the pass achieved, including when a batch fails partway:
/// each batch is its own autocommit statement, so rows deleted before the
/// failure are permanently gone and the count has to survive alongside the
/// error — the same reasoning as [`crate::audit::AuditLogger::purge_before`].
/// Auditing `rows_removed = 0` after a partial sweep would understate a
/// deletion that really happened.
/// The pooled Postgres handle `AppState::pool()` hands out. Aliased locally
/// (as `job.rs` does) rather than imported, so a build without Postgres never
/// names a diesel type.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
type PgPool = diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>;

#[cfg(all(feature = "db", not(feature = "sqlite")))]
async fn sweep_postgres(
    pool: &PgPool,
    dataset: RetentionDataset,
    window: Duration,
    dry_run: bool,
) -> Result<SweptDataset, SweepFailure> {
    use diesel_async::RunQueryDsl as _;

    let Some((table, predicate)) = stale_row_predicate(dataset) else {
        return Err(sweep_failed(
            0,
            format!("{dataset} is not a sweep-enforced dataset"),
        ));
    };
    let key = sweep_key_column(dataset);
    let mut conn = pool.get().await.map_err(|e| {
        sweep_failed(
            0,
            format!("retention sweep could not acquire a connection: {e}"),
        )
    })?;

    let cutoff = resolve_cutoff(&mut conn, table, window).await?;
    let eligible = count_stale(&mut conn, table, predicate, cutoff, 0).await?;

    if dry_run || eligible == 0 {
        return Ok(SweptDataset {
            cutoff,
            eligible,
            removed: 0,
            truncated: false,
        });
    }

    // Batched deletes: never one unbounded DELETE over a hot table.
    //
    // The predicate is repeated on the OUTER delete, not only in the
    // id sub-select. Without it the delete's only qual is `id IN {…}`, so a
    // row that stops qualifying between the sub-select and the delete — an
    // operator replaying a dead letter back to `enqueued` mid-sweep — would
    // be deleted while live. Repeating the predicate makes the row's
    // re-checked qual under READ COMMITTED include its current state.
    let mut removed: u64 = 0;
    for _ in 0..SWEEP_MAX_BATCHES {
        let affected = diesel::sql_query(format!(
            "DELETE FROM {table} WHERE {predicate} AND {key} IN \
             (SELECT {key} FROM {table} WHERE {predicate} LIMIT $2)"
        ))
        .bind::<diesel::sql_types::Timestamptz, _>(cutoff)
        .bind::<diesel::sql_types::BigInt, _>(SWEEP_BATCH_ROWS)
        .execute(&mut *conn)
        .await
        // `removed`, not 0: each batch is its own autocommit statement, so
        // everything earlier batches deleted is permanently gone even though
        // this one failed.
        .map_err(|e| {
            sweep_failed_at(
                cutoff,
                removed,
                format!("retention sweep of {table} failed: {e}"),
            )
        })?;
        removed = removed.saturating_add(u64::try_from(affected).unwrap_or(u64::MAX));
        // Stop only on a batch that deleted *nothing*, not merely fewer than
        // `SWEEP_BATCH_ROWS`. A short batch does not mean the table is
        // drained: the outer predicate is re-checked at delete time, so rows
        // that stopped qualifying between the sub-select and the delete (a
        // dead letter replayed back to `enqueued`, a refreshed tracking row)
        // make the batch short while thousands of stale rows remain.
        if affected == 0 {
            break;
        }
    }

    // Ask the database what is left rather than inferring it from the last
    // batch size. Inferring gets it wrong in both directions: a short batch
    // reports a complete sweep that isn't, and an exact multiple of
    // `SWEEP_BATCH_ROWS * SWEEP_MAX_BATCHES` reports a truncated sweep that
    // is actually done. `truncated` ends up in the audit record, so a wrong
    // answer here is a compliance trail claiming enforcement that did not
    // happen.
    let remaining = count_stale(&mut conn, table, predicate, cutoff, removed).await?;

    Ok(SweptDataset {
        cutoff,
        eligible,
        removed,
        truncated: remaining > 0,
    })
}

/// Pair an error with the rows the pass had already deleted when it hit that
/// error, so no failure path can silently drop the count.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
fn sweep_failed(removed: u64, message: String) -> SweepFailure {
    SweepFailure {
        cutoff: None,
        removed,
        error: Box::new(crate::AutumnError::internal_server_error_msg(message)),
    }
}

/// As [`sweep_failed`], for a failure that happened *after* the database
/// resolved the cutoff.
///
/// The committed deletes used that cutoff, so the report and the audit record
/// must state it — not the provisional app-clock value `run_one_dataset`
/// seeded, which under clock skew names a different deletion boundary from
/// the one actually applied.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
fn sweep_failed_at(cutoff: DateTime<Utc>, removed: u64, message: String) -> SweepFailure {
    SweepFailure {
        cutoff: Some(cutoff),
        removed,
        error: Box::new(crate::AutumnError::internal_server_error_msg(message)),
    }
}

/// A sweep pass that failed, carrying everything the pass still has to
/// report: the cutoff the database applied (once known), the rows earlier
/// autocommit batches already deleted, and the error itself.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
struct SweepFailure {
    cutoff: Option<DateTime<Utc>>,
    removed: u64,
    /// Boxed to keep the `Err` variant of this module's sweep `Result`s small:
    /// `AutumnError` is wide enough that carrying it inline trips
    /// `clippy::result_large_err` on every function that returns one.
    error: Box<crate::AutumnError>,
}

/// Resolve `NOW() - window` **on the database server**.
///
/// An out-of-range window is an error, never a silent clamp: clamping would
/// compute a cutoff *closer* to now than the configured window while the
/// report still advertised the configured value, deleting rows the report
/// claimed were inside the policy. `RetentionConfig::validate` rejects such a
/// window at boot, so this is a defensive backstop.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
async fn resolve_cutoff(
    conn: &mut diesel_async::AsyncPgConnection,
    table: &str,
    window: Duration,
) -> Result<DateTime<Utc>, SweepFailure> {
    use diesel_async::RunQueryDsl as _;

    #[derive(diesel::QueryableByName)]
    struct CutoffRow {
        #[diesel(sql_type = diesel::sql_types::Timestamptz)]
        cutoff: DateTime<Utc>,
    }

    let Ok(window_secs) = u32::try_from(window.as_secs()) else {
        return Err(sweep_failed(
            0,
            format!(
                "retention window for {table} ({}s) exceeds the maximum the sweep can \
                 apply; refusing rather than silently sweeping a shorter window",
                window.as_secs()
            ),
        ));
    };

    // Seconds bound as a parameter rather than a formatted interval literal,
    // so nothing about the window is ever interpolated into SQL text.
    diesel::sql_query("SELECT NOW() - make_interval(secs => $1) AS cutoff")
        .bind::<diesel::sql_types::Double, _>(f64::from(window_secs))
        .get_result::<CutoffRow>(conn)
        .await
        .map(|row| row.cutoff)
        .map_err(|e| {
            sweep_failed(
                0,
                format!("retention could not resolve the cutoff for {table}: {e}"),
            )
        })
}

/// Count the rows `predicate` currently matches.
///
/// `removed_so_far` rides along so a failure here still reports the rows an
/// earlier batch loop already deleted.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
async fn count_stale(
    conn: &mut diesel_async::AsyncPgConnection,
    table: &str,
    predicate: &str,
    cutoff: DateTime<Utc>,
    removed_so_far: u64,
) -> Result<u64, SweepFailure> {
    use diesel_async::RunQueryDsl as _;

    #[derive(diesel::QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        count: i64,
    }

    diesel::sql_query(format!(
        "SELECT COUNT(*) AS count FROM {table} WHERE {predicate}"
    ))
    .bind::<diesel::sql_types::Timestamptz, _>(cutoff)
    .get_result::<CountRow>(conn)
    .await
    .map(|row| u64::try_from(row.count).unwrap_or(0))
    .map_err(|e| {
        sweep_failed_at(
            cutoff,
            removed_so_far,
            format!("retention count for {table} failed: {e}"),
        )
    })
}

/// What one Postgres sweep pass achieved.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
struct SweptDataset {
    /// The instant the database itself resolved for the window.
    cutoff: DateTime<Utc>,
    /// Rows matching the predicate before the pass ran.
    eligible: u64,
    /// Rows actually deleted.
    removed: u64,
    /// `true` when stale rows are still present after the pass.
    truncated: bool,
}

// ── Audit record (AC #6) ─────────────────────────────────────────────────

/// Write the auditable record for one dataset's pass: dataset, cutoff, and
/// rows removed (AC #6).
///
/// Recorded for every real sweep of a bounded dataset, including one that
/// removed zero rows — "we enforced the policy and there was nothing to
/// delete" is a claim a compliance reviewer needs evidence for, and the
/// volume is bounded by the number of configured datasets times the sweep
/// interval.
///
/// Deliberately *not* recorded for:
///
/// - a dry run, which deletes nothing and so is not a sweep;
/// - a dataset with no window, or one whose backend expires records itself —
///   there is no deletion to attribute;
///
/// but a dataset held back by a **legal hold** *is* recorded even though
/// nothing was deleted: "the policy wanted to delete this and did not" is
/// exactly what a reviewer needs to see.
async fn audit_sweep(state: &AppState, report: &RetentionDatasetReport) {
    let is_legal_hold = report
        .skipped
        .as_deref()
        .is_some_and(|reason| reason.starts_with("legal hold:"));
    let swept = report.eligible_rows.is_some() || report.error.is_some();
    if report.dry_run || (!swept && !is_legal_hold) {
        return;
    }

    tracing::info!(
        target: "autumn.audit",
        dataset = %report.dataset,
        cutoff = report.cutoff.as_deref().unwrap_or("-"),
        eligible_rows = report.eligible_rows.unwrap_or(0),
        rows_removed = report.rows_removed,
        truncated = report.truncated,
        skipped = report.skipped.as_deref().unwrap_or(""),
        error = report.error.as_deref().unwrap_or(""),
        "retention_sweep"
    );

    let status = if report.error.is_some() {
        crate::audit::AuditStatus::Failure
    } else {
        crate::audit::AuditStatus::Success
    };
    let mut event = crate::audit::AuditEvent::new(
        RETENTION_AUDIT_ACTOR,
        RETENTION_AUDIT_ACTION,
        report.dataset.clone(),
        None,
        status,
    )
    .with_metadata("dataset", report.dataset.clone())
    .with_metadata("cutoff", report.cutoff.clone().unwrap_or_default())
    .with_metadata("rows_removed", report.rows_removed.to_string())
    .with_metadata("truncated", report.truncated.to_string());
    if let Some(eligible) = report.eligible_rows {
        event = event.with_metadata("eligible_rows", eligible.to_string());
    }
    if let Some(skipped) = report.skipped.as_deref() {
        event = event.with_metadata("skipped", skipped);
    }
    if let Some(error) = report.error.as_deref() {
        event = event.with_metadata("error", error);
    }

    if let Err(error) = crate::audit::write_from_state(state, event).await {
        tracing::warn!(
            error = %error,
            dataset = %report.dataset,
            "retention sweep audit record could not be written"
        );
    }
}

// ── Scheduled task (AC #2) ───────────────────────────────────────────────

/// The recurring in-process sweep, or `None` when no dataset declares a
/// window.
///
/// Returning `None` is what makes AC #1's "leaving a dataset unset preserves
/// today's behavior exactly" structural rather than a promise each sweeper
/// has to keep: with nothing configured, no task is registered, no scheduler
/// loop is spawned, and no query is ever issued.
///
/// Registered as a [`TaskCoordination::Fleet`] task so a multi-replica
/// deployment runs the sweep once per tick, not once per replica.
#[must_use]
pub fn framework_retention_task(config: &RetentionConfig) -> Option<TaskInfo> {
    if !config.any_window_configured() {
        return None;
    }
    Some(TaskInfo {
        name: RETENTION_SWEEP_TASK_NAME.to_owned(),
        schedule: Schedule::FixedDelay(config.sweep_interval_duration()),
        coordination: TaskCoordination::Fleet,
        handler: |state| {
            Box::pin(async move {
                let reports = run_retention(&state, &RetentionRunOptions::default()).await?;
                for report in &reports {
                    if let Some(error) = report.error.as_deref() {
                        tracing::warn!(
                            dataset = %report.dataset,
                            error,
                            "framework retention sweep failed for one dataset"
                        );
                    }
                }
                Ok(())
            })
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_datasets_defaults_to_every_dataset() {
        assert_eq!(
            resolve_datasets(None).expect("no filter always resolves"),
            RETENTION_DATASETS.to_vec()
        );
    }

    #[test]
    fn resolve_datasets_rejects_an_unknown_key_and_lists_the_valid_ones() {
        let error = resolve_datasets(Some("jobs")).expect_err("a typo must fail loudly");
        let message = error.to_string();
        assert!(message.contains("jobs"), "{message}");
        assert!(
            message.contains("job_history"),
            "the error must list the valid keys: {message}"
        );
    }

    #[test]
    fn job_history_never_matches_a_live_job_row() {
        // Regression guard for the single most dangerous thing this module
        // could get wrong: a predicate that reaches an enqueued, running, or
        // retry-scheduled job.
        //
        // Asserts on the *set* of statuses rather than a substring of the
        // rendered SQL: a substring match breaks the moment a new terminal
        // status is added (as `discarded` was), which tells you nothing about
        // whether the predicate is still safe.
        let (table, predicate) =
            stale_row_predicate(RetentionDataset::JobHistory).expect("job_history is sweepable");
        assert_eq!(table, "autumn_jobs");
        let statuses = statuses_in(predicate);
        assert_eq!(
            statuses,
            vec!["completed", "discarded", "failed"],
            "job_history must match every terminal status and no live one: {predicate}"
        );
        for live in ["enqueued", "running"] {
            assert!(
                !statuses.contains(&live.to_owned()),
                "{live:?} is a live status and must never be swept: {predicate}"
            );
        }
        assert!(predicate.contains("finished_at IS NOT NULL"), "{predicate}");
        assert!(
            predicate.contains("unique_window = 'ttl'"),
            "a TTL-window dedup token must be excluded: {predicate}"
        );
    }

    #[test]
    fn commit_hooks_sweeps_every_terminal_hook_status() {
        let (table, predicate) =
            stale_row_predicate(RetentionDataset::CommitHooks).expect("commit_hooks is sweepable");
        assert_eq!(table, "autumn_repository_commit_hooks");
        assert_eq!(
            statuses_in(predicate),
            vec!["after_hook_failed", "completed", "failed"],
            "every status that sets finished_at must be swept, or those rows \
             grow forever despite a configured window: {predicate}"
        );
        assert!(predicate.contains("finished_at IS NOT NULL"), "{predicate}");
    }

    #[test]
    fn experiment_assignments_protect_every_restartable_state() {
        let (table, predicate) = stale_row_predicate(RetentionDataset::ExperimentAssignments)
            .expect("experiment_assignments is sweepable");
        assert_eq!(table, "autumn_experiment_assignments");
        assert_eq!(
            states_in(predicate),
            vec!["concluded", "draft", "running"],
            "an assignment may only be swept once its experiment can never run \
             again. `ExperimentService::start` restores draft OR concluded to \
             running and refuses only archived, so all three belong in the \
             protected set — dropping `concluded` (as this predicate originally \
             did) lets a restart re-bucket every returning actor: {predicate}"
        );
        assert!(
            !states_in(predicate).contains(&"archived".to_owned()),
            "archived is the one terminal state and must stay collectable: {predicate}"
        );
    }

    /// The sorted set of `'quoted'` state literals named by a predicate's
    /// `e.state IN (...)` clause.
    fn states_in(predicate: &str) -> Vec<String> {
        let needle = "e.state IN (";
        let start = predicate
            .find(needle)
            .map(|at| at + needle.len())
            .expect("predicate names a state set");
        let end = start + predicate[start..].find(')').expect("closing paren");
        let mut states: Vec<String> = predicate[start..end]
            .split(',')
            .map(|state| state.trim().trim_matches('\'').to_owned())
            .collect();
        states.sort();
        states
    }

    /// The sorted set of `'quoted'` status literals named by a predicate's
    /// `status IN (...)` clause.
    fn statuses_in(predicate: &str) -> Vec<String> {
        let start = predicate
            .find("status IN (")
            .map(|at| at + "status IN (".len())
            .expect("predicate names a status set");
        let end = start + predicate[start..].find(')').expect("closing paren");
        let mut statuses: Vec<String> = predicate[start..end]
            .split(',')
            .map(|status| status.trim().trim_matches('\'').to_owned())
            .collect();
        statuses.sort();
        statuses
    }

    #[test]
    fn ttl_enforced_datasets_have_no_sweep_predicate() {
        for dataset in RETENTION_DATASETS {
            if dataset.enforcement() == RetentionEnforcement::Sweep {
                assert!(stale_row_predicate(dataset).is_some(), "{dataset}");
            } else {
                assert!(
                    stale_row_predicate(dataset).is_none(),
                    "{dataset} is not sweep-enforced but has a sweep predicate"
                );
            }
        }
    }
}
