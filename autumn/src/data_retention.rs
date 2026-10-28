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

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::AutumnResult;
use crate::config::{AutumnConfig, RetentionConfig};
use crate::gdpr::{ErasureStrategy, GdprRegistry};
use crate::state::AppState;
use crate::task::{Schedule, TaskCoordination, TaskInfo};
use std::fmt::Write as _;

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
#[cfg(feature = "db")]
const SWEEP_BATCH_ROWS: i64 = 500;

/// Upper bound on batches per dataset per run. A run that hits this stops and
/// resumes on the next tick, so a first sweep of a years-old table is spread
/// over several ticks instead of one very long transaction storm.
#[cfg(feature = "db")]
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
    /// A framework-owned store on disk is pruned of records the window no
    /// longer keeps, plus anything already orphaned.
    StorePrune,
}

impl std::fmt::Display for RetentionEnforcement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Sweep => "sweep",
            Self::BackendTtl => "backend ttl",
            Self::ArchiveRewrite => "archive rewrite",
            Self::StorePrune => "store prune",
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
    /// Tenant custom-domain registry records and their certificates.
    CustomDomains,
}

/// Every framework-owned dataset, in the order the CLI reports them.
///
/// The single list the config surface, the sweeps, the CLI report and the
/// docs table all derive from.
pub const RETENTION_DATASETS: [RetentionDataset; 9] = [
    RetentionDataset::JobHistory,
    RetentionDataset::CommitHooks,
    RetentionDataset::JobTracking,
    RetentionDataset::Idempotency,
    RetentionDataset::ExperimentAssignments,
    RetentionDataset::WebhookReplay,
    RetentionDataset::Sessions,
    RetentionDataset::AuditArchives,
    RetentionDataset::CustomDomains,
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
            Self::CustomDomains => "custom_domains",
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
            Self::CustomDomains => "Tenant custom-domain registry records and their certificates",
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
            Self::Idempotency
            | Self::WebhookReplay
            | Self::Sessions
            | Self::AuditArchives
            | Self::CustomDomains => None,
        }
    }

    /// The table that holds this dataset's records for `config`, or `None`
    /// when they live elsewhere.
    ///
    /// `job_history` and `job_tracking` follow `jobs.backend`. Under Redis or
    /// the in-memory queue, their records are not in the framework table. A
    /// sweep or a table-keyed legal hold cannot reach them there.
    #[must_use]
    pub fn table_for(self, config: &AutumnConfig) -> Option<&'static str> {
        // The `sqlite` runtime keeps both tables in the app's own file.
        if matches!(self, Self::JobHistory | Self::JobTracking)
            && !matches!(config.jobs.backend.as_str(), "postgres" | "sqlite")
        {
            return None;
        }
        self.table()
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
            Self::CustomDomains => RetentionEnforcement::StorePrune,
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
            // A connected domain is kept for as long as it is registered; only
            // a connection abandoned before DNS was ever published ages out.
            Self::CustomDomains => "forever (until the app offboards the domain)",
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
            | Self::AuditArchives
            | Self::CustomDomains => None,
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
/// a GDPR registration names a table. The database idempotency store keeps
/// its rows in `autumn_idempotency_keys` and expires them itself, outside
/// this engine: it checks [`table_legal_hold`] for that table, as the
/// job-tracking cleanup does for its own.
#[must_use]
pub fn legal_hold_for(
    dataset: RetentionDataset,
    registry: Option<&GdprRegistry>,
) -> Option<String> {
    table_legal_hold(dataset.table()?, registry)
}

/// The legal-hold reason for `table`, if it is under a legal hold.
///
/// A table is held when the [`GdprRegistry`] registers it with
/// [`ErasureStrategy::Retain`]. For code that deletes from a table outside
/// the retention engine: it must not delete while this is `Some`.
#[must_use]
pub fn table_legal_hold(table: &str, registry: Option<&GdprRegistry>) -> Option<String> {
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

/// Which datasets one run covers.
///
/// The scheduler splits the work by where the data lives. Shared data must
/// be swept once per tick. Per-process data must be swept on every replica.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RetentionScope {
    /// Every dataset. The CLI uses this.
    #[default]
    All,
    /// Datasets in shared stores (one run per fleet tick).
    Shared,
    /// Datasets in this process's local files (one run per replica).
    PerReplica,
}

impl RetentionScope {
    const fn covers(self, enforcement: RetentionEnforcement) -> bool {
        let per_replica = matches!(enforcement, RetentionEnforcement::ArchiveRewrite);
        match self {
            Self::All => true,
            Self::Shared => !per_replica,
            Self::PerReplica => per_replica,
        }
    }
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
    run_retention_for(state, options, RetentionScope::All).await
}

/// Like [`run_retention`], limited to the datasets in `scope`.
///
/// # Errors
///
/// Same as [`run_retention`].
pub async fn run_retention_for(
    state: &AppState,
    options: &RetentionRunOptions<'_>,
    scope: RetentionScope,
) -> AutumnResult<Vec<RetentionDatasetReport>> {
    let datasets = resolve_datasets(options.dataset)?;
    let config = state.config_arc();
    let mut reports = Vec::with_capacity(datasets.len());
    for dataset in datasets
        .into_iter()
        .filter(|dataset| scope.covers(dataset.enforcement()))
    {
        let mut report = run_one_dataset(state, &config, dataset, options.dry_run).await;
        audit_sweep(state, &mut report).await;
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
    let started = crate::time::ambient_monotonic();
    let effective = effective_retention(config, dataset);
    // A provisional cutoff, so a dataset with no database still reports one.
    // The sweep path replaces it with the instant Postgres itself resolved —
    // see `sweep_postgres`.
    let cutoff = effective
        .window
        .and_then(|window| chrono::Duration::from_std(window).ok())
        .map(|window| crate::time::ambient_now() - window);

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

    // The sweep deletes from a table. Off Postgres, job history is not in it.
    // Say so, so a zero is not read as "already clean".
    if dataset == RetentionDataset::JobHistory && dataset.table_for(config).is_none() {
        report.skipped = Some(format!(
            "NOT enforced: jobs.backend = {:?} keeps finished jobs outside autumn_jobs, \
             so this sweep cannot reach them",
            config.jobs.backend
        ));
        report.duration_ms = elapsed_ms(started);
        return report;
    }

    // A legal hold outranks the window: check it before doing any counting,
    // so a held dataset never even reads the rows it must not delete.
    let registry = state.extension::<GdprRegistry>();
    if let Some(reason) = legal_hold_for(dataset, registry.as_deref()) {
        report.skipped = Some(if dataset.table_for(config).is_some() {
            format!("legal hold: {reason}")
        } else {
            // The hold names a table that does not hold these records. The
            // record TTL still applies, so the hold does not protect them.
            format!(
                "legal hold: {reason}. NOT preserved: jobs.backend = {:?} stores these \
                 records outside the held table, and the window shortens their TTL",
                config.jobs.backend
            )
        });
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
                registry.as_deref(),
            ));
        }
        RetentionEnforcement::ArchiveRewrite => {
            apply_archive_purge(state, cutoff, dry_run, &mut report).await;
        }
        RetentionEnforcement::StorePrune => {
            apply_custom_domain_prune(state, cutoff, dry_run, &mut report).await;
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
fn backend_ttl_note(
    dataset: RetentionDataset,
    config: &AutumnConfig,
    window_secs: u64,
    registry: Option<&GdprRegistry>,
) -> String {
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
    // The database store skips its own cleanup while its table is held.
    let held = (dataset == RetentionDataset::Idempotency
        && config.idempotency.backend == crate::config::IdempotencyBackend::Database)
        .then(|| table_legal_hold("autumn_idempotency_keys", registry))
        .flatten();
    if let Some(reason) = held {
        let _ = write!(
            note,
            ". NOT removed: autumn_idempotency_keys is under legal hold: {reason}. The \
             store keeps expired rows until the hold ends"
        );
    } else if evicts_lazily(dataset, config) {
        note.push_str(
            ". With the built-in store, expired records are not served. The store \
             removes them only on later writes. A quiet process can keep them past the \
             window. Use redis to remove them on time",
        );
    }
    if dataset == RetentionDataset::Sessions {
        note.push_str(
            ", and a custom SessionStore installed with AppBuilder::with_session_store must \
             apply the window itself",
        );
    }
    note
}

/// `true` when the dataset uses a built-in store that removes expired records
/// only on later writes.
fn evicts_lazily(dataset: RetentionDataset, config: &AutumnConfig) -> bool {
    match dataset {
        // The memory and database stores both sweep on every 128th write.
        RetentionDataset::Idempotency => {
            config.idempotency.backend != crate::config::IdempotencyBackend::Redis
        }
        RetentionDataset::WebhookReplay => {
            config.security.webhooks.replay.backend == crate::webhook::WebhookReplayBackend::Memory
        }
        // An unknown backend falls back to the local runtime and its
        // in-memory tracking store.
        RetentionDataset::JobTracking => !matches!(
            config.jobs.backend.as_str(),
            "postgres" | "redis" | "sqlite"
        ),
        _ => false,
    }
}

fn elapsed_ms(started: crate::time::MonotonicInstant) -> u64 {
    u64::try_from(
        crate::time::ambient_monotonic()
            .saturating_duration_since(started)
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Prune abandoned custom-domain registrations and orphaned certificates
/// through the installed [`CustomDomainPruner`](crate::custom_domain::CustomDomainPruner).
///
/// Reports "not enabled" rather than a zero when custom domains are off, so a
/// configured window never reads as an enforced policy over nothing.
#[cfg(not(feature = "custom-domains"))]
#[allow(clippy::unused_async)]
async fn apply_custom_domain_prune(
    _state: &AppState,
    _cutoff: DateTime<Utc>,
    _dry_run: bool,
    report: &mut RetentionDatasetReport,
) {
    report.skipped = Some("custom domains are not compiled in (feature `custom-domains`)".to_owned());
}

#[cfg(feature = "custom-domains")]
async fn apply_custom_domain_prune(
    state: &AppState,
    cutoff: DateTime<Utc>,
    dry_run: bool,
    report: &mut RetentionDatasetReport,
) {
    let Some(pruner) =
        state.extension::<std::sync::Arc<dyn crate::custom_domain::CustomDomainPruner>>()
    else {
        report.skipped =
            Some("custom domains are not enabled ([server.tls.acme.custom_domains])".to_owned());
        return;
    };
    match pruner.prune(cutoff.timestamp(), dry_run).await {
        Ok(removed) => report.rows_removed = removed,
        Err(e) => report.error = Some(e),
    }
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
        | RetentionDataset::AuditArchives
        | RetentionDataset::CustomDomains => None,
    }
}

/// The primary-key column each sweepable table batches by.
#[cfg(feature = "db")]
const fn sweep_key_column(dataset: RetentionDataset) -> &'static str {
    match dataset {
        RetentionDataset::JobTracking => "key",
        _ => "id",
    }
}

#[cfg(feature = "db")]
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
    #[cfg(not(feature = "sqlite"))]
    let outcome = sweep_postgres(&pool, dataset, window, dry_run).await;
    #[cfg(feature = "sqlite")]
    let outcome = match sweep_sqlite(&pool, dataset, window, dry_run).await {
        Ok(SqliteSweep::Swept(swept)) => Ok(swept),
        Ok(SqliteSweep::MissingTable(table)) => {
            report.skipped = Some(format!(
                "{table} does not exist in this database, so this dataset has no rows here"
            ));
            return;
        }
        Ok(SqliteSweep::Skipped(reason)) => {
            report.skipped = Some(reason);
            return;
        }
        Err(failure) => Err(failure),
    };
    match outcome {
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
            report.eligible_rows = failure.eligible;
            report.rows_removed = failure.removed;
            report.truncated = true;
            report.error = Some(failure.error.to_string());
        }
    }
}

/// A build without the `db` feature has no database to sweep. Report that
/// rather than pretending a sweep ran.
#[cfg(not(feature = "db"))]
#[allow(clippy::unused_async)]
async fn apply_sweep(
    _state: &AppState,
    _dataset: RetentionDataset,
    _window: Duration,
    _dry_run: bool,
    report: &mut RetentionDatasetReport,
) {
    report.skipped = Some("this build has no database support (`db` feature off)".to_owned());
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
    let eligible = count_stale(&mut conn, table, predicate, cutoff, 0, None).await?;

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
                Some(eligible),
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
    let remaining =
        count_stale(&mut conn, table, predicate, cutoff, removed, Some(eligible)).await?;

    Ok(SweptDataset {
        cutoff,
        eligible,
        removed,
        truncated: remaining > 0,
    })
}

/// Pair an error with the rows the pass had already deleted when it hit that
/// error, so no failure path can silently drop the count.
#[cfg(feature = "db")]
fn sweep_failed(removed: u64, message: String) -> SweepFailure {
    SweepFailure {
        cutoff: None,
        removed,
        eligible: None,
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
#[cfg(feature = "db")]
fn sweep_failed_at(
    cutoff: DateTime<Utc>,
    removed: u64,
    eligible: Option<u64>,
    message: String,
) -> SweepFailure {
    SweepFailure {
        cutoff: Some(cutoff),
        removed,
        eligible,
        error: Box::new(crate::AutumnError::internal_server_error_msg(message)),
    }
}

/// A sweep pass that failed, carrying everything the pass still has to
/// report: the cutoff the database applied (once known), the rows earlier
/// autocommit batches already deleted, and the error itself.
#[cfg(feature = "db")]
struct SweepFailure {
    cutoff: Option<DateTime<Utc>>,
    removed: u64,
    /// Rows counted before the batches ran. `None` when the failure came first.
    eligible: Option<u64>,
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
/// A failure here still reports `removed_so_far` and the earlier `eligible`
/// count, if any.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
async fn count_stale(
    conn: &mut diesel_async::AsyncPgConnection,
    table: &str,
    predicate: &str,
    cutoff: DateTime<Utc>,
    removed_so_far: u64,
    eligible: Option<u64>,
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
            eligible,
            format!("retention count for {table} failed: {e}"),
        )
    })
}

/// What one sweep pass achieved.
#[cfg(feature = "db")]
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

// ── SQLite sweep (issue #2553) ───────────────────────────────────────────

/// The database clock in epoch milliseconds. Like the Postgres sweep, the
/// cutoff is read on the database side. The job runtime's own prune uses the
/// app clock, so the two differ only under a simulated clock.
#[cfg(any(test, all(feature = "db", feature = "sqlite")))]
const SQLITE_NOW_MS: &str = "CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)";

/// How a `SQLite` dataset stores the time its predicate compares.
#[cfg(any(test, all(feature = "db", feature = "sqlite")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SqliteClock {
    /// `BIGINT` epoch milliseconds (`autumn_jobs`).
    Millis,
    /// `TEXT` as `YYYY-MM-DD HH:MM:SS`, UTC (`CURRENT_TIMESTAMP`).
    Text,
}

/// The `SQLite` counterpart of [`stale_row_predicate`]: table, predicate and
/// clock. `?1` is the cutoff. The rules match the Postgres predicates and the
/// job runtime's own history prune.
#[cfg(any(test, all(feature = "db", feature = "sqlite")))]
#[must_use]
fn stale_row_predicate_sqlite(
    dataset: RetentionDataset,
) -> Option<(&'static str, String, SqliteClock)> {
    match dataset {
        RetentionDataset::JobHistory => Some((
            "autumn_jobs",
            format!(
                "status IN ('completed', 'failed', 'discarded') \
                 AND finished_at IS NOT NULL AND finished_at < ?1 \
                 AND NOT (unique_key IS NOT NULL AND unique_window = 'ttl' \
                          AND enqueued_at > ({SQLITE_NOW_MS}) - COALESCE(unique_ttl_ms, 0))"
            ),
            SqliteClock::Millis,
        )),
        RetentionDataset::CommitHooks => Some((
            "autumn_repository_commit_hooks",
            "status IN ('completed', 'failed', 'after_hook_failed') \
             AND finished_at IS NOT NULL AND finished_at < ?1"
                .to_owned(),
            SqliteClock::Text,
        )),
        RetentionDataset::ExperimentAssignments => Some((
            "autumn_experiment_assignments",
            "assigned_at < ?1 AND NOT EXISTS ( \
                 SELECT 1 FROM autumn_experiments e \
                 WHERE e.name = autumn_experiment_assignments.experiment \
                   AND e.state IN ('draft', 'running', 'concluded'))"
                .to_owned(),
            SqliteClock::Text,
        )),
        // `job_tracking` is a backend-TTL dataset on `SQLite`: the job runtime
        // deletes expired rows itself.
        RetentionDataset::JobTracking
        | RetentionDataset::Idempotency
        | RetentionDataset::WebhookReplay
        | RetentionDataset::Sessions
        | RetentionDataset::AuditArchives
        | RetentionDataset::CustomDomains => None,
    }
}

/// The index that serves a dataset's cutoff scan, created if it is missing.
/// `autumn_jobs` already has `idx_autumn_jobs_status_finished`.
#[cfg(any(test, all(feature = "db", feature = "sqlite")))]
const fn sqlite_sweep_index(dataset: RetentionDataset) -> Option<&'static str> {
    match dataset {
        RetentionDataset::CommitHooks => Some(
            "CREATE INDEX IF NOT EXISTS idx_autumn_commit_hooks_finished \
             ON autumn_repository_commit_hooks (finished_at) \
             WHERE status IN ('completed', 'failed', 'after_hook_failed')",
        ),
        RetentionDataset::ExperimentAssignments => Some(
            "CREATE INDEX IF NOT EXISTS idx_autumn_exp_assignments_assigned_at \
             ON autumn_experiment_assignments (assigned_at)",
        ),
        _ => None,
    }
}

/// The resolved cutoff, in the form the dataset's column stores.
#[cfg(all(feature = "db", feature = "sqlite"))]
enum SqliteCutoff {
    Millis(i64),
    Text(String),
}

/// What a `SQLite` sweep did, or why it had nothing to do.
#[cfg(all(feature = "db", feature = "sqlite"))]
enum SqliteSweep {
    Swept(SweptDataset),
    /// A dataset table this database does not have.
    MissingTable(&'static str),
    /// The table exists but its schema is too old to sweep safely.
    Skipped(String),
}

#[cfg(all(feature = "db", feature = "sqlite"))]
#[derive(diesel::QueryableByName)]
struct SqliteCount {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

/// Run (or dry-run) one dataset's sweep against the app's `SQLite` file.
///
/// Same contract as the Postgres sweep: the cutoff is resolved on the
/// database, deletes run in bounded batches as separate statements, the outer
/// `DELETE` repeats the predicate so a row revived mid-sweep survives, and a
/// failure reports what earlier batches already removed.
#[cfg(all(feature = "db", feature = "sqlite"))]
// One pass: look, resolve, count, index, delete in batches, recount.
#[allow(clippy::too_many_lines)]
async fn sweep_sqlite(
    pool: &diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>,
    dataset: RetentionDataset,
    window: Duration,
    dry_run: bool,
) -> Result<SqliteSweep, SweepFailure> {
    use diesel_async::RunQueryDsl as _;

    let Some((table, predicate, clock)) = stale_row_predicate_sqlite(dataset) else {
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

    for needed in std::iter::once(table)
        .chain((dataset == RetentionDataset::ExperimentAssignments).then_some("autumn_experiments"))
    {
        if !sqlite_table_exists(&mut conn, needed).await? {
            return Ok(SqliteSweep::MissingTable(needed));
        }
    }

    // The job runtime adds `unique_ttl_ms` when it starts. A sweep that runs
    // first on an older file must not guess: skip it and delete nothing.
    if dataset == RetentionDataset::JobHistory
        && !sqlite_column_exists(&mut conn, table, "unique_ttl_ms").await?
    {
        return Ok(SqliteSweep::Skipped(format!(
            "{table} predates the unique_ttl_ms column, so the sweep cannot check TTL holds. \
             Start the app once so the job runtime upgrades the table, then run this again"
        )));
    }

    let (cutoff_at, cutoff) = resolve_sqlite_cutoff(&mut conn, table, window, clock).await?;
    let eligible =
        count_stale_sqlite(&mut conn, table, &predicate, &cutoff, 0, None, cutoff_at).await?;
    if dry_run || eligible == 0 {
        return Ok(SqliteSweep::Swept(SweptDataset {
            cutoff: cutoff_at,
            eligible,
            removed: 0,
            truncated: false,
        }));
    }

    // The same indexes the Postgres migration adds. The SQLite migration for
    // them is a no-op shim that existing files have already applied, so the
    // sweep creates them. Without one, every batch rescans the table.
    if let Some(index) = sqlite_sweep_index(dataset) {
        diesel::sql_query(index)
            .execute(&mut *conn)
            .await
            .map_err(|e| {
                sweep_failed_at(
                    cutoff_at,
                    0,
                    Some(eligible),
                    format!("retention could not create the sweep index on {table}: {e}"),
                )
            })?;
    }

    let mut removed: u64 = 0;
    for _ in 0..SWEEP_MAX_BATCHES {
        let sql = format!(
            "DELETE FROM {table} WHERE {predicate} AND {key} IN \
             (SELECT {key} FROM {table} WHERE {predicate} LIMIT ?2)"
        );
        let query = diesel::sql_query(sql);
        let affected = match &cutoff {
            SqliteCutoff::Millis(ms) => {
                query
                    .bind::<diesel::sql_types::BigInt, _>(*ms)
                    .bind::<diesel::sql_types::BigInt, _>(SWEEP_BATCH_ROWS)
                    .execute(&mut *conn)
                    .await
            }
            SqliteCutoff::Text(text) => {
                query
                    .bind::<diesel::sql_types::Text, _>(text.clone())
                    .bind::<diesel::sql_types::BigInt, _>(SWEEP_BATCH_ROWS)
                    .execute(&mut *conn)
                    .await
            }
        }
        .map_err(|e| {
            sweep_failed_at(
                cutoff_at,
                removed,
                Some(eligible),
                format!("retention sweep of {table} failed: {e}"),
            )
        })?;
        removed = removed.saturating_add(u64::try_from(affected).unwrap_or(u64::MAX));
        // Stop on a batch that deleted nothing, not a short one: the outer
        // predicate is re-checked at delete time, so a short batch does not
        // mean the table is drained.
        if affected == 0 {
            break;
        }
    }

    let remaining = count_stale_sqlite(
        &mut conn,
        table,
        &predicate,
        &cutoff,
        removed,
        Some(eligible),
        cutoff_at,
    )
    .await?;
    Ok(SqliteSweep::Swept(SweptDataset {
        cutoff: cutoff_at,
        eligible,
        removed,
        truncated: remaining > 0,
    }))
}

#[cfg(all(feature = "db", feature = "sqlite"))]
async fn sqlite_table_exists(
    conn: &mut crate::db::PooledConnection,
    table: &str,
) -> Result<bool, SweepFailure> {
    use diesel_async::RunQueryDsl as _;

    diesel::sql_query(
        "SELECT COUNT(*) AS count FROM sqlite_master WHERE type = 'table' AND name = ?1",
    )
    .bind::<diesel::sql_types::Text, _>(table)
    .get_result::<SqliteCount>(&mut **conn)
    .await
    .map(|row| row.count > 0)
    .map_err(|e| sweep_failed(0, format!("retention could not look for {table}: {e}")))
}

#[cfg(all(feature = "db", feature = "sqlite"))]
async fn sqlite_column_exists(
    conn: &mut crate::db::PooledConnection,
    table: &str,
    column: &str,
) -> Result<bool, SweepFailure> {
    use diesel_async::RunQueryDsl as _;

    diesel::sql_query("SELECT COUNT(*) AS count FROM pragma_table_info(?1) WHERE name = ?2")
        .bind::<diesel::sql_types::Text, _>(table)
        .bind::<diesel::sql_types::Text, _>(column)
        .get_result::<SqliteCount>(&mut **conn)
        .await
        .map(|row| row.count > 0)
        .map_err(|e| sweep_failed(0, format!("retention could not inspect {table}: {e}")))
}

/// Resolve `now - window` **on the database**, as an instant and in the form
/// the dataset's column stores.
///
/// An out-of-range window is an error, never a clamp: clamping would delete
/// rows the report claims are inside the policy.
#[cfg(all(feature = "db", feature = "sqlite"))]
async fn resolve_sqlite_cutoff(
    conn: &mut crate::db::PooledConnection,
    table: &str,
    window: Duration,
    clock: SqliteClock,
) -> Result<(DateTime<Utc>, SqliteCutoff), SweepFailure> {
    use diesel_async::RunQueryDsl as _;

    #[derive(diesel::QueryableByName)]
    struct CutoffRow {
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
        cutoff_ms: Option<i64>,
    }

    let too_wide = || {
        sweep_failed(
            0,
            format!(
                "retention window for {table} ({}s) exceeds the maximum the sweep can \
                 apply; refusing rather than silently sweeping a shorter window",
                window.as_secs()
            ),
        )
    };
    let window_ms = i64::try_from(window.as_millis()).map_err(|_| too_wide())?;
    let row = diesel::sql_query(format!("SELECT {SQLITE_NOW_MS} - ?1 AS cutoff_ms"))
        .bind::<diesel::sql_types::BigInt, _>(window_ms)
        .get_result::<CutoffRow>(&mut **conn)
        .await
        .map_err(|e| {
            sweep_failed(
                0,
                format!("retention could not resolve the cutoff for {table}: {e}"),
            )
        })?;
    let ms = row.cutoff_ms.ok_or_else(too_wide)?;
    // A text column holds whole seconds. Report the instant that is applied.
    let at = match clock {
        SqliteClock::Millis => DateTime::<Utc>::from_timestamp_millis(ms),
        SqliteClock::Text => DateTime::<Utc>::from_timestamp(ms.div_euclid(1000), 0),
    }
    .ok_or_else(too_wide)?;
    let cutoff = match clock {
        SqliteClock::Millis => SqliteCutoff::Millis(ms),
        SqliteClock::Text => SqliteCutoff::Text(at.format("%Y-%m-%d %H:%M:%S").to_string()),
    };
    Ok((at, cutoff))
}

#[cfg(all(feature = "db", feature = "sqlite"))]
async fn count_stale_sqlite(
    conn: &mut crate::db::PooledConnection,
    table: &str,
    predicate: &str,
    cutoff: &SqliteCutoff,
    removed_so_far: u64,
    eligible: Option<u64>,
    cutoff_at: DateTime<Utc>,
) -> Result<u64, SweepFailure> {
    use diesel_async::RunQueryDsl as _;

    let query = diesel::sql_query(format!(
        "SELECT COUNT(*) AS count FROM {table} WHERE {predicate}"
    ));
    match cutoff {
        SqliteCutoff::Millis(ms) => {
            query
                .bind::<diesel::sql_types::BigInt, _>(*ms)
                .get_result::<SqliteCount>(&mut **conn)
                .await
        }
        SqliteCutoff::Text(text) => {
            query
                .bind::<diesel::sql_types::Text, _>(text.clone())
                .get_result::<SqliteCount>(&mut **conn)
                .await
        }
    }
    .map(|row| u64::try_from(row.count).unwrap_or(0))
    .map_err(|e| {
        sweep_failed_at(
            cutoff_at,
            removed_so_far,
            eligible,
            format!("retention count for {table} failed: {e}"),
        )
    })
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
///
/// If the write fails, the error text is added to
/// [`RetentionDatasetReport::error`]. Rows can already be gone, so the CLI
/// exits non-zero and the task fails. The text starts with "audit record" to
/// separate it from a sweep error.
async fn audit_sweep(state: &AppState, report: &mut RetentionDatasetReport) {
    let is_legal_hold = report
        .skipped
        .as_deref()
        .is_some_and(|reason| reason.starts_with("legal hold:"));
    let swept = report.eligible_rows.is_some() || report.error.is_some();
    if report.dry_run || (!swept && !is_legal_hold) {
        return;
    }

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
        tracing::error!(
            error = %error,
            dataset = %report.dataset,
            rows_removed = report.rows_removed,
            "retention sweep audit record could not be written"
        );
        let audit_failure = format!("audit record not written: {error}");
        report.error = Some(match report.error.take() {
            Some(sweep_failure) => format!("{sweep_failure}; {audit_failure}"),
            None => audit_failure,
        });
    }

    // Emitted last, so the fallback trail carries an audit-write failure too.
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
}

// ── Purge guard ──────────────────────────────────────────────────────────

/// Why an on-demand purge must stop, if it must.
///
/// The app checks this with the profile its own config loader resolved. The
/// CLI also checks, as a fast pre-flight, but it only knows the profile name
/// it passes in. A custom loader can pick another one. `profile` is that
/// resolved name. `force` is the operator's `--force`.
#[must_use]
pub fn purge_refusal(profile: Option<&str>, force: bool) -> Option<String> {
    if force {
        return None;
    }
    let name = profile.map(str::trim);
    if name.is_some_and(|name| {
        ["dev", "development", "test"]
            .iter()
            .any(|safe| name.eq_ignore_ascii_case(safe))
    }) {
        return None;
    }
    Some(format!(
        "refusing to purge framework data: the app resolved the {} profile. The configured \
         [retention] policy already runs inside the app. Re-run with --force to purge now",
        name.map_or_else(|| "an unknown".to_owned(), |name| format!("{name:?}"))
    ))
}

// ── Scheduled task (AC #2) ───────────────────────────────────────────────

/// Turn dataset errors into one task error.
///
/// The caller runs this after all datasets finish, so one failure does not
/// stop the others. The scheduler records the task as failed.
fn fail_on_dataset_errors(reports: &[RetentionDatasetReport]) -> AutumnResult<()> {
    let failures: Vec<String> = reports
        .iter()
        .filter_map(|report| {
            report
                .error
                .as_deref()
                .map(|error| format!("{}: {error}", report.dataset))
        })
        .collect();
    if failures.is_empty() {
        return Ok(());
    }
    Err(crate::AutumnError::internal_server_error_msg(format!(
        "retention sweep failed for {} dataset(s): {}",
        failures.len(),
        failures.join("; ")
    )))
}

/// Name of the per-replica archive sweep task.
pub const RETENTION_ARCHIVE_TASK_NAME: &str = "autumn-retention-archive-sweep";

/// Whether any configured window needs the shared (fleet) sweep.
fn shared_window_configured(config: &RetentionConfig) -> bool {
    config.windows().iter().any(|(key, window)| {
        window.is_some()
            && RetentionDataset::from_key(key)
                .is_none_or(|dataset| RetentionScope::Shared.covers(dataset.enforcement()))
    })
}

/// The recurring in-process sweep of shared data, or `None` when no such
/// dataset declares a window.
///
/// Returning `None` is what makes AC #1's "leaving a dataset unset preserves
/// today's behavior exactly" structural rather than a promise each sweeper
/// has to keep: with nothing configured, no task is registered, no scheduler
/// loop is spawned, and no query is ever issued.
///
/// Registered as a [`TaskCoordination::Fleet`] task so a multi-replica
/// deployment runs the sweep once per tick, not once per replica. The audit
/// archive is not in it: see [`framework_archive_retention_task`].
#[must_use]
pub fn framework_retention_task(config: &RetentionConfig) -> Option<TaskInfo> {
    if !shared_window_configured(config) {
        return None;
    }
    Some(TaskInfo {
        name: RETENTION_SWEEP_TASK_NAME.to_owned(),
        schedule: Schedule::FixedDelay(config.sweep_interval_duration()),
        coordination: TaskCoordination::Fleet,
        handler: |state| {
            Box::pin(async move {
                let reports = run_retention_for(
                    &state,
                    &RetentionRunOptions::default(),
                    RetentionScope::Shared,
                )
                .await?;
                fail_on_dataset_errors(&reports)
            })
        },
    })
}

/// Every scheduled retention task the config needs: the fleet sweep, the
/// per-replica archive sweep, both, or none.
#[must_use]
pub fn framework_retention_tasks(config: &RetentionConfig) -> Vec<TaskInfo> {
    framework_retention_task(config)
        .into_iter()
        .chain(framework_archive_retention_task(config))
        .collect()
}

/// The recurring sweep of the local audit archive, or `None` when
/// `retention.audit_archives` is unset.
///
/// The archive is a file on this replica's disk. A fleet-coordinated task
/// would purge one replica's file and leave the others to grow. This task
/// uses [`TaskCoordination::PerReplica`], so every replica purges its own.
#[must_use]
pub fn framework_archive_retention_task(config: &RetentionConfig) -> Option<TaskInfo> {
    config.audit_archives.as_ref()?;
    Some(TaskInfo {
        name: RETENTION_ARCHIVE_TASK_NAME.to_owned(),
        schedule: Schedule::FixedDelay(config.sweep_interval_duration()),
        coordination: TaskCoordination::PerReplica,
        handler: |state| {
            Box::pin(async move {
                let reports = run_retention_for(
                    &state,
                    &RetentionRunOptions::default(),
                    RetentionScope::PerReplica,
                )
                .await?;
                fail_on_dataset_errors(&reports)
            })
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::audit::{AuditError, AuditEvent, AuditLogger, AuditSink};
    use std::sync::Arc;

    struct FailingSink;

    impl AuditSink for FailingSink {
        fn write(
            &self,
            _event: AuditEvent,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AuditError>> + Send + '_>>
        {
            Box::pin(async { Err(AuditError::new("disk full")) })
        }
    }

    fn held_report() -> RetentionDatasetReport {
        RetentionDatasetReport {
            dataset: "job_history".to_owned(),
            description: String::new(),
            enforcement: "sweep".to_owned(),
            window_secs: Some(86_400),
            source: "[retention]".to_owned(),
            cutoff: Some("2026-01-01T00:00:00+00:00".to_owned()),
            eligible_rows: Some(3),
            rows_removed: 3,
            truncated: false,
            dry_run: false,
            skipped: None,
            duration_ms: 0,
            error: None,
        }
    }

    fn state_with_sink(sink: impl AuditSink) -> AppState {
        let state = AppState::for_test();
        state.insert_extension(AuditLogger::new().with_sink(Arc::new(sink)));
        state
    }

    #[tokio::test]
    async fn a_failed_audit_write_sets_the_report_error() {
        let state = state_with_sink(FailingSink);
        let mut report = held_report();
        audit_sweep(&state, &mut report).await;
        let error = report.error.expect("audit failure must reach the report");
        assert!(error.contains("audit record"), "{error}");
        assert!(error.contains("disk full"), "{error}");
        assert_eq!(report.rows_removed, 3, "removed rows stay reported");
    }

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buf").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = Self;
        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }

    #[tokio::test]
    async fn the_sweep_trace_carries_the_audit_write_failure() {
        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(subscriber));
        let state = state_with_sink(FailingSink);
        let mut report = held_report();
        audit_sweep(&state, &mut report).await;
        let output = String::from_utf8(buf.0.lock().expect("buf").clone()).expect("utf8");
        let line = output
            .lines()
            .find(|line| line.contains("retention_sweep"))
            .expect("sweep trace line");
        assert!(line.contains("audit record not written"), "{line}");
    }

    #[tokio::test]
    async fn a_failed_legal_hold_audit_write_sets_the_report_error() {
        let state = state_with_sink(FailingSink);
        let mut report = held_report();
        report.eligible_rows = None;
        report.rows_removed = 0;
        report.skipped = Some("legal hold: tax records".to_owned());
        audit_sweep(&state, &mut report).await;
        assert!(report.error.is_some(), "{report:?}");
    }

    #[tokio::test]
    async fn a_failed_audit_write_keeps_the_sweep_error() {
        let state = state_with_sink(FailingSink);
        let mut report = held_report();
        report.error = Some("statement timeout".to_owned());
        audit_sweep(&state, &mut report).await;
        let error = report.error.expect("error");
        assert!(error.contains("statement timeout"), "{error}");
        assert!(error.contains("disk full"), "{error}");
    }

    #[tokio::test]
    async fn a_written_audit_record_leaves_the_report_clean() {
        let state = AppState::for_test();
        let mut report = held_report();
        audit_sweep(&state, &mut report).await;
        assert_eq!(report.error, None);
    }

    #[tokio::test]
    async fn a_dry_run_never_fails_on_the_audit_sink() {
        let state = state_with_sink(FailingSink);
        let mut report = held_report();
        report.dry_run = true;
        audit_sweep(&state, &mut report).await;
        assert_eq!(report.error, None);
    }

    fn config_with_backend(backend: &str) -> AutumnConfig {
        let mut config = AutumnConfig::default();
        config.jobs.backend = backend.to_owned();
        config.retention.job_history = Some("90d".to_owned());
        config.retention.job_tracking = Some("7d".to_owned());
        config
    }

    #[tokio::test]
    async fn job_history_is_not_enforced_on_a_redis_or_local_jobs_backend() {
        for backend in ["redis", "local"] {
            let config = config_with_backend(backend);
            let report = run_one_dataset(
                &AppState::for_test(),
                &config,
                RetentionDataset::JobHistory,
                false,
            )
            .await;
            let skipped = report.skipped.expect("the sweep cannot reach these jobs");
            assert!(skipped.contains("NOT enforced"), "{backend}: {skipped}");
            assert!(skipped.contains(backend), "{backend}: {skipped}");
            assert_eq!(report.rows_removed, 0);
        }
    }

    #[tokio::test]
    async fn job_history_is_not_reported_unenforced_on_postgres_or_sqlite() {
        for backend in ["postgres", "sqlite"] {
            let config = config_with_backend(backend);
            let report = run_one_dataset(
                &AppState::for_test(),
                &config,
                RetentionDataset::JobHistory,
                true,
            )
            .await;
            let skipped = report.skipped.unwrap_or_default();
            assert!(!skipped.contains("NOT enforced"), "{backend}: {skipped}");
        }
    }

    #[tokio::test]
    async fn a_legal_hold_is_not_claimed_for_records_outside_the_held_table() {
        let state = AppState::for_test();
        let registry = GdprRegistry::new().register(crate::gdpr::ModelRegistration::retain(
            "autumn_job_tracking",
            "litigation",
        ));
        state.insert_extension(registry);

        let redis = config_with_backend("redis");
        let report = run_one_dataset(&state, &redis, RetentionDataset::JobTracking, true).await;
        let skipped = report.skipped.expect("held");
        assert!(skipped.starts_with("legal hold:"), "{skipped}");
        assert!(skipped.contains("NOT preserved"), "{skipped}");

        let postgres = config_with_backend("postgres");
        let report = run_one_dataset(&state, &postgres, RetentionDataset::JobTracking, true).await;
        let skipped = report.skipped.expect("held");
        assert!(!skipped.contains("NOT preserved"), "{skipped}");
    }

    #[test]
    fn only_jobs_datasets_depend_on_the_jobs_backend() {
        let postgres = config_with_backend("postgres");
        let redis = config_with_backend("redis");
        let sensitive: Vec<&str> = RETENTION_DATASETS
            .iter()
            .filter(|dataset| dataset.table_for(&postgres) != dataset.table_for(&redis))
            .map(|dataset| dataset.key())
            .collect();
        assert_eq!(sensitive, ["job_history", "job_tracking"]);
    }

    #[tokio::test]
    async fn the_shared_scope_leaves_the_archive_to_each_replica() {
        let state = AppState::for_test();
        let options = RetentionRunOptions::default();
        let shared = run_retention_for(&state, &options, RetentionScope::Shared)
            .await
            .expect("run");
        let local = run_retention_for(&state, &options, RetentionScope::PerReplica)
            .await
            .expect("run");
        assert!(shared.iter().all(|r| r.dataset != "audit_archives"));
        assert_eq!(
            local.iter().map(|r| r.dataset.as_str()).collect::<Vec<_>>(),
            ["audit_archives"]
        );
        let all = run_retention(&state, &options).await.expect("run");
        assert_eq!(all.len(), shared.len() + local.len());
    }

    #[test]
    fn purge_is_refused_unless_the_app_resolved_a_dev_or_test_profile() {
        for profile in [Some("dev"), Some("development"), Some("test"), Some("Dev")] {
            assert_eq!(purge_refusal(profile, false), None, "{profile:?}");
        }
        for profile in [Some("prod"), Some("production"), Some("staging"), None] {
            let refusal = purge_refusal(profile, false).expect("must refuse");
            assert!(refusal.contains("--force"), "{refusal}");
        }
    }

    #[test]
    fn force_lifts_the_purge_refusal() {
        assert_eq!(purge_refusal(Some("prod"), true), None);
        assert_eq!(purge_refusal(None, true), None);
    }

    #[test]
    fn the_task_list_follows_the_configured_windows() {
        let names = |config: &RetentionConfig| {
            framework_retention_tasks(config)
                .into_iter()
                .map(|task| task.name)
                .collect::<Vec<_>>()
        };
        let mut config = RetentionConfig::default();
        assert!(names(&config).is_empty());
        config.audit_archives = Some("400d".to_owned());
        assert_eq!(names(&config), [RETENTION_ARCHIVE_TASK_NAME]);
        config.job_history = Some("90d".to_owned());
        assert_eq!(
            names(&config),
            [RETENTION_SWEEP_TASK_NAME, RETENTION_ARCHIVE_TASK_NAME]
        );
    }

    #[test]
    fn fail_on_dataset_errors_passes_when_clean() {
        assert!(fail_on_dataset_errors(&[held_report()]).is_ok());
    }

    #[test]
    fn fail_on_dataset_errors_names_every_failed_dataset() {
        let mut first = held_report();
        first.error = Some("timeout".to_owned());
        let mut second = held_report();
        second.dataset = "commit_hooks".to_owned();
        second.error = Some("denied".to_owned());
        let ok = held_report();
        let message = fail_on_dataset_errors(&[first, ok, second])
            .expect_err("a failed dataset must fail the task")
            .to_string();
        assert!(message.contains("job_history"), "{message}");
        assert!(message.contains("commit_hooks"), "{message}");
        assert!(message.contains("timeout"), "{message}");
    }

    #[test]
    fn in_memory_stores_warn_about_late_eviction() {
        let mut config = AutumnConfig::default();
        config.idempotency.backend = crate::config::IdempotencyBackend::Memory;
        config.security.webhooks.replay.backend = crate::webhook::WebhookReplayBackend::Memory;
        config.jobs.backend = "local".to_owned();
        for dataset in [
            RetentionDataset::Idempotency,
            RetentionDataset::WebhookReplay,
            RetentionDataset::JobTracking,
        ] {
            let note = backend_ttl_note(dataset, &config, 60, None);
            assert!(note.contains("built-in store"), "{dataset}: {note}");
            assert!(note.contains("quiet process"), "{dataset}: {note}");
        }
    }

    #[test]
    fn shared_stores_do_not_warn_about_late_eviction() {
        let mut config = AutumnConfig::default();
        config.idempotency.backend = crate::config::IdempotencyBackend::Redis;
        config.security.webhooks.replay.backend = crate::webhook::WebhookReplayBackend::Redis;
        config.jobs.backend = "redis".to_owned();
        for dataset in [
            RetentionDataset::Idempotency,
            RetentionDataset::WebhookReplay,
            RetentionDataset::JobTracking,
        ] {
            let note = backend_ttl_note(dataset, &config, 60, None);
            assert!(!note.contains("quiet process"), "{dataset}: {note}");
        }
    }

    #[test]
    fn only_the_dataset_with_an_in_memory_store_warns() {
        let mut config = AutumnConfig::default();
        config.idempotency.backend = crate::config::IdempotencyBackend::Memory;
        config.security.webhooks.replay.backend = crate::webhook::WebhookReplayBackend::Redis;
        config.jobs.backend = "redis".to_owned();
        let warns =
            |dataset| backend_ttl_note(dataset, &config, 60, None).contains("quiet process");
        assert!(warns(RetentionDataset::Idempotency));
        assert!(!warns(RetentionDataset::WebhookReplay));
        assert!(!warns(RetentionDataset::JobTracking));
    }

    #[test]
    fn the_database_idempotency_store_warns_and_sqlite_jobs_do_not() {
        let mut config = AutumnConfig::default();
        config.idempotency.backend = crate::config::IdempotencyBackend::Database;
        assert!(evicts_lazily(RetentionDataset::Idempotency, &config));
        config.jobs.backend = "sqlite".to_owned();
        assert!(!evicts_lazily(RetentionDataset::JobTracking, &config));
    }

    #[test]
    fn an_unknown_jobs_backend_warns_because_it_falls_back_to_memory() {
        let mut config = AutumnConfig::default();
        config.jobs.backend = "postgresql".to_owned();
        assert!(evicts_lazily(RetentionDataset::JobTracking, &config));
        config.jobs.backend = "redis".to_owned();
        assert!(!evicts_lazily(RetentionDataset::JobTracking, &config));
    }

    #[test]
    fn a_held_database_idempotency_table_is_not_described_as_swept() {
        let mut config = AutumnConfig::default();
        config.idempotency.backend = crate::config::IdempotencyBackend::Database;
        let registry = GdprRegistry::new().register(crate::gdpr::ModelRegistration::retain(
            "autumn_idempotency_keys",
            "litigation",
        ));
        let note = backend_ttl_note(RetentionDataset::Idempotency, &config, 60, Some(&registry));
        assert!(note.contains("legal hold: litigation"), "{note}");
        assert!(!note.contains("quiet process"), "{note}");
    }

    #[test]
    fn the_memory_session_note_keeps_its_own_verdict() {
        let mut config = AutumnConfig::default();
        config.session.backend = crate::session::SessionBackend::Memory;
        let note = backend_ttl_note(RetentionDataset::Sessions, &config, 60, None);
        assert!(note.starts_with("NOT enforced"), "{note}");
        assert!(!note.contains("quiet process"), "{note}");
    }

    #[cfg(feature = "db")]
    #[test]
    fn a_failure_after_the_count_keeps_the_eligible_count() {
        let failure = sweep_failed_at(Utc::now(), 500, Some(5_000_000), "timeout".to_owned());
        assert_eq!(failure.eligible, Some(5_000_000));
        assert_eq!(failure.removed, 500);
        assert_eq!(sweep_failed(0, "no pool".to_owned()).eligible, None);
    }

    #[test]
    fn sqlite_predicates_use_one_cutoff_and_the_same_statuses_as_postgres() {
        for dataset in RETENTION_DATASETS {
            let sqlite = stale_row_predicate_sqlite(dataset);
            let pg = stale_row_predicate(dataset);
            assert_eq!(
                sqlite.as_ref().map(|(table, ..)| *table),
                pg.map(|(table, _)| table)
                    .filter(|_| dataset != RetentionDataset::JobTracking),
                "{dataset}: same table set, job_tracking is TTL-only on SQLite"
            );
            if let Some((_, predicate, _)) = sqlite {
                assert!(predicate.contains("?1"), "{dataset}: {predicate}");
                assert!(
                    !predicate.contains("$1") && !predicate.contains("?2"),
                    "{predicate}"
                );
                if let Some((_, pg_predicate)) = pg {
                    if pg_predicate.contains("status IN (") {
                        assert_eq!(
                            statuses_in(&predicate),
                            statuses_in(pg_predicate),
                            "{dataset}"
                        );
                    }
                    if pg_predicate.contains("state IN (") {
                        assert_eq!(states_in(&predicate), states_in(pg_predicate), "{dataset}");
                    }
                }
            }
        }
    }

    #[test]
    fn every_sqlite_sweep_has_an_index_or_one_already_exists() {
        for dataset in RETENTION_DATASETS {
            let Some(index) = sqlite_sweep_index(dataset) else {
                continue;
            };
            let (table, ..) = stale_row_predicate_sqlite(dataset).expect("a sweep");
            assert!(index.contains(table), "{dataset}: {index}");
            assert!(index.contains("IF NOT EXISTS"), "{index}");
        }
        assert!(sqlite_sweep_index(RetentionDataset::CommitHooks).is_some());
        assert!(sqlite_sweep_index(RetentionDataset::ExperimentAssignments).is_some());
    }

    #[test]
    fn sqlite_job_history_keeps_a_live_ttl_dedup_hold() {
        let (_, predicate, clock) =
            stale_row_predicate_sqlite(RetentionDataset::JobHistory).expect("sweepable");
        assert_eq!(clock, SqliteClock::Millis);
        assert!(predicate.contains("unique_ttl_ms"), "{predicate}");
        assert!(predicate.contains("finished_at IS NOT NULL"), "{predicate}");
    }

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
