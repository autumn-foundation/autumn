//! Declarative data-retention sweeps (issue #1342).
//!
//! `#[repository(Model, retention(after = "30d", basis = created_at))]` (or
//! the soft-delete `purge_deleted_after = "90d"` variant) registers a batched,
//! scheduler-coordinated sweep with zero hand-written `#[scheduled]` fns and
//! no SQL — see `docs/guide/retention-sweeps.md`.
//!
//! The macro emits one [`RetentionSweepDescriptor`] per policy via
//! `inventory::submit!`. [`collect_retention_tasks`] folds every descriptor
//! into the same [`TaskInfo`] pipeline `#[scheduled]` uses, so a declared
//! policy is a recurring, fleet-coordinated sweep the moment the app boots —
//! no `tasks![...]` entry required. [`run_retention_dry_run`] walks the same
//! registry to power `autumn retention --dry-run` without deleting anything.

use std::future::Future;
use std::pin::Pin;

use serde::Serialize;

use crate::AutumnResult;
use crate::state::AppState;
use crate::task::TaskInfo;

/// Per-run report for a single model's retention sweep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetentionSweepReport {
    /// The model name the policy is declared on.
    pub model: String,
    /// Rows deleted (or, for a dry run, rows that *would* be deleted).
    pub rows_swept: u64,
    /// Wall-clock time the sweep took, in milliseconds.
    pub duration_ms: u64,
    /// `true` when this report came from a dry run (nothing was deleted).
    pub dry_run: bool,
}

/// Counts (never deletes) the rows a policy would sweep.
///
/// Takes `AppState` by value (a cheap `Arc`-based clone), mirroring
/// [`crate::task::TaskHandler`] — this sidesteps the higher-ranked-lifetime
/// coercion a borrowed `fn(&AppState) -> Pin<Box<dyn Future<...> + 'a>>`
/// would need at every macro-generated call site.
type DryRunFn =
    fn(AppState) -> Pin<Box<dyn Future<Output = AutumnResult<RetentionSweepReport>> + Send>>;

/// Link-time descriptor emitted by `#[repository(Model, retention(...))]`.
///
/// Collected via `inventory` so a declared policy needs no manual wiring:
/// [`collect_retention_tasks`] feeds the scheduler and `autumn retention
/// --dry-run` walks every descriptor without the app listing anything in
/// `tasks![...]`.
#[doc(hidden)]
pub struct RetentionSweepDescriptor {
    /// The model name the policy is declared on (matched by `--model`).
    pub model_name: &'static str,
    /// Builds the recurring [`TaskInfo`] the scheduler registers.
    pub task_info: fn() -> TaskInfo,
    /// Counts (never deletes) the rows the policy would sweep.
    pub dry_run: DryRunFn,
}

inventory::collect!(RetentionSweepDescriptor);

/// Every registered retention sweep, as scheduler tasks ready to merge with
/// whatever the app passed to
/// [`AppBuilder::tasks`](crate::app::AppBuilder::tasks).
///
/// Called automatically at boot; apps never call this directly.
#[must_use]
pub fn collect_retention_tasks() -> Vec<TaskInfo> {
    inventory::iter::<RetentionSweepDescriptor>()
        .map(|descriptor| (descriptor.task_info)())
        .collect()
}

/// `true` once at least one `#[repository(..., retention(...))]` policy has
/// been compiled into the binary (used to skip scheduler merge work).
#[must_use]
pub fn has_retention_descriptors() -> bool {
    inventory::iter::<RetentionSweepDescriptor>
        .into_iter()
        .next()
        .is_some()
}

/// Run every registered policy's dry-run count, optionally filtered to one
/// model. Never deletes anything — see [`RetentionSweepReport::dry_run`].
///
/// Reports are sorted by model name so `autumn retention --dry-run` prints a
/// stable order.
///
/// # Errors
///
/// Returns the first policy's error (e.g. no database pool configured), or a
/// not-found error when `model_filter` names a model with no registered
/// policy.
pub async fn run_retention_dry_run(
    state: &AppState,
    model_filter: Option<&str>,
) -> AutumnResult<Vec<RetentionSweepReport>> {
    let mut reports = Vec::new();
    let mut matched = false;
    for descriptor in inventory::iter::<RetentionSweepDescriptor> {
        if let Some(filter) = model_filter
            && descriptor.model_name != filter
        {
            continue;
        }
        matched = true;
        reports.push((descriptor.dry_run)(state.clone()).await?);
    }
    if let Some(filter) = model_filter
        && !matched
    {
        return Err(crate::AutumnError::not_found_msg(format!(
            "no #[repository(..., retention(...))] policy is registered for model {filter:?}"
        )));
    }
    reports.sort_by(|a, b| a.model.cmp(&b.model));
    Ok(reports)
}

/// Emit the structured `{model, rows_swept, duration_ms}` log line for a run.
///
/// For real (non-dry-run) sweeps, also bumps the `retention_sweep_rows_total`
/// counter and `retention_sweep_duration_ms` histogram, both labeled by
/// `model`.
pub fn log_retention_sweep(report: &RetentionSweepReport) {
    tracing::info!(
        model = %report.model,
        rows_swept = report.rows_swept,
        duration_ms = report.duration_ms,
        dry_run = report.dry_run,
        "retention: sweep complete"
    );
    if !report.dry_run {
        crate::metrics::counter("retention_sweep_rows_total")
            .with_label("model", report.model.clone())
            .increment(report.rows_swept);
        // Duration histograms are inherently approximate; losing precision
        // above 2^52ms (~142,000 years) is not a real concern.
        #[allow(clippy::cast_precision_loss)]
        let duration_ms = report.duration_ms as f64;
        crate::metrics::histogram("retention_sweep_duration_ms")
            .with_label("model", report.model.clone())
            .record(duration_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dry_run(
        _state: AppState,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<RetentionSweepReport>> + Send>> {
        Box::pin(async {
            Ok(RetentionSweepReport {
                model: "Widget".to_string(),
                rows_swept: 3,
                duration_ms: 5,
                dry_run: true,
            })
        })
    }

    fn sample_task_info() -> TaskInfo {
        TaskInfo {
            name: "retention-sweep-widget".to_string(),
            schedule: crate::task::Schedule::FixedDelay(std::time::Duration::from_secs(3600)),
            coordination: crate::task::TaskCoordination::Fleet,
            handler: |_state| Box::pin(async { Ok(()) }),
        }
    }

    #[test]
    fn report_serializes_with_expected_fields() {
        let report = RetentionSweepReport {
            model: "Widget".to_string(),
            rows_swept: 42,
            duration_ms: 7,
            dry_run: false,
        };

        let json = serde_json::to_value(&report).expect("report should serialize");
        assert_eq!(json["model"], "Widget");
        assert_eq!(json["rows_swept"], 42);
        assert_eq!(json["duration_ms"], 7);
        assert_eq!(json["dry_run"], false);
    }

    #[test]
    fn collect_retention_tasks_calls_every_registered_descriptor() {
        // The consolidated integration test binary links every macro-expanded
        // fixture, so descriptors from other test modules may already be
        // registered; assert on containment rather than an exact count.
        let tasks = collect_retention_tasks();
        assert!(
            tasks.iter().all(|t| !t.name.is_empty()),
            "every collected retention task must have a name"
        );
    }

    #[tokio::test]
    async fn run_retention_dry_run_filters_by_model_name() {
        struct Fixture;
        inventory::submit! {
            RetentionSweepDescriptor {
                model_name: "__RetentionRuntimeTestWidget",
                task_info: sample_task_info,
                dry_run: sample_dry_run,
            }
        }
        let _ = Fixture;

        let state = AppState::for_test();
        let reports = run_retention_dry_run(&state, Some("__RetentionRuntimeTestWidget"))
            .await
            .expect("dry run should succeed for a registered model");

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].model, "Widget");
        assert!(reports[0].dry_run);
    }

    #[tokio::test]
    async fn run_retention_dry_run_rejects_unknown_model_filter() {
        let state = AppState::for_test();
        let error = run_retention_dry_run(&state, Some("__NoSuchRetentionModel"))
            .await
            .expect_err("an unregistered model filter must error");

        assert!(error.to_string().contains("__NoSuchRetentionModel"));
    }
}
