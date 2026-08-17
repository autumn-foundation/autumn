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
    ///
    /// Not globally unique — see [`table`](Self::table), which is.
    pub model: String,
    /// The table the policy's model is backed by. Schema-unique, unlike
    /// `model`, so two same-named models in different modules
    /// (`auth::Session`, `admin::Session`) still print as distinguishable
    /// rows in an unfiltered `autumn retention --dry-run` and label distinct
    /// `retention_sweep_*` metric series instead of merging into one.
    pub table: String,
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
    ///
    /// Not globally unique — two modules can declare same-named models
    /// (`auth::Session`, `admin::Session`), each with its own policy. Prefer
    /// [`table_name`](Self::table_name) to disambiguate; see
    /// [`run_retention_dry_run`].
    pub model_name: &'static str,
    /// The table the policy's model is backed by — schema-unique, unlike
    /// `model_name`, and identical to the table-qualified component of the
    /// generated task name (`retention-sweep-<table_name>`). Also matched by
    /// `--model`, so a caller can pass this to disambiguate two models that
    /// share a short name.
    pub table_name: &'static str,
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
///
/// # Panics
///
/// Panics if two policies produce the same task name. The generated name is
/// table-qualified (`retention-sweep-<table_name>`), which disambiguates two
/// same-named models in different modules, but does NOT disambiguate two
/// *different* `#[repository(...)]` declarations that both target the same
/// table (a supported pattern — see e.g. `live_broadcast.rs`'s multiple
/// repositories over one table) and both declare `retention(...)`. Rather
/// than silently merging their scheduler/actuator state — which can make
/// one policy's fleet-coordinated run skip in favor of the other's — this
/// fails loudly at boot, matching every other "declared an unsupported
/// combination" retention rejection, just checked here (at the one point
/// with visibility across every `#[repository(...)]` in the binary) instead
/// of at macro-expansion time.
#[must_use]
pub fn collect_retention_tasks() -> Vec<TaskInfo> {
    let tasks: Vec<TaskInfo> = inventory::iter::<RetentionSweepDescriptor>()
        .map(|descriptor| (descriptor.task_info)())
        .collect();
    if let Some(message) = duplicate_retention_task_name(&tasks) {
        panic!("{message}");
    }
    tasks
}

/// The panic message for [`collect_retention_tasks`], or `None` if every
/// task name in `tasks` is unique. A free function (rather than inlined
/// into `collect_retention_tasks`) so the collision case is unit-testable
/// against a synthetic `Vec<TaskInfo>` — testing it through
/// `collect_retention_tasks` itself would mean `inventory::submit!`-ing a
/// colliding pair into the process-global registry, which every other test
/// in the binary shares and can't un-register.
fn duplicate_retention_task_name(tasks: &[TaskInfo]) -> Option<String> {
    let mut seen = std::collections::HashSet::with_capacity(tasks.len());
    tasks.iter().find_map(|task| {
        (!seen.insert(task.name.as_str())).then(|| {
            format!(
                "two #[repository(..., retention(...))] policies both produced the retention \
                 task name {:?} — most likely two different repositories declaring \
                 retention(...) on the same table. Only one repository per table may declare \
                 retention(...) for now.",
                task.name
            )
        })
    })
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
/// `model_filter` matches against either `model_name` or `table_name`. Two
/// different modules can declare same-named models (`auth::Session`,
/// `admin::Session`), each with its own policy, so a `model_name` match is
/// not necessarily unique — pass the table name instead (from the generated
/// task name, `retention-sweep-<table_name>`) to disambiguate; an ambiguous
/// `model_name` filter errors rather than silently running every match.
///
/// Reports are sorted by model name, then table name, so `autumn retention
/// --dry-run` prints a stable order even when two policies share a model
/// name.
///
/// # Errors
///
/// Returns the first policy's error (e.g. no database pool configured), a
/// not-found error when `model_filter` names nothing registered, or a
/// bad-request error when `model_filter` matches more than one policy.
pub async fn run_retention_dry_run(
    state: &AppState,
    model_filter: Option<&str>,
) -> AutumnResult<Vec<RetentionSweepReport>> {
    let mut reports = Vec::new();
    if let Some(filter) = model_filter {
        let matches: Vec<&RetentionSweepDescriptor> = inventory::iter::<RetentionSweepDescriptor>
            .into_iter()
            .filter(|descriptor| descriptor.model_name == filter || descriptor.table_name == filter)
            .collect();
        if matches.is_empty() {
            return Err(crate::AutumnError::not_found_msg(format!(
                "no #[repository(..., retention(...))] policy is registered for model {filter:?}"
            )));
        }
        if matches.len() > 1 {
            let table_names: Vec<&str> = matches.iter().map(|d| d.table_name).collect();
            return Err(crate::AutumnError::bad_request_msg(format!(
                "{filter:?} matches more than one retention policy ({table_names:?}); pass the \
                 table name instead of the model name to disambiguate, e.g. --model {}",
                table_names[0]
            )));
        }
        reports.push((matches[0].dry_run)(state.clone()).await?);
    } else {
        for descriptor in inventory::iter::<RetentionSweepDescriptor> {
            reports.push((descriptor.dry_run)(state.clone()).await?);
        }
    }
    reports.sort_by(|a, b| a.model.cmp(&b.model).then_with(|| a.table.cmp(&b.table)));
    Ok(reports)
}

/// Emit the structured `{model, table, rows_swept, duration_ms}` log line for
/// a run.
///
/// For real (non-dry-run) sweeps, also bumps the `retention_sweep_rows_total`
/// counter and `retention_sweep_duration_seconds` timer, both labeled by
/// `model` *and* `table` — the table label is what keeps two same-named
/// models in different modules (`auth::Session`, `admin::Session`) from
/// merging into one metric series. The timer is named `*_seconds` and
/// records seconds — the framework's own Prometheus convention (see
/// `autumn_web::metrics`) — even though the log line and
/// [`RetentionSweepReport`] use milliseconds, which is what AC7 (issue
/// #1342) names.
pub fn log_retention_sweep(report: &RetentionSweepReport) {
    tracing::info!(
        model = %report.model,
        table = %report.table,
        rows_swept = report.rows_swept,
        duration_ms = report.duration_ms,
        dry_run = report.dry_run,
        "retention: sweep complete"
    );
    if !report.dry_run {
        crate::metrics::counter("retention_sweep_rows_total")
            .with_label("model", report.model.clone())
            .with_label("table", report.table.clone())
            .increment(report.rows_swept);
        crate::metrics::timer("retention_sweep_duration_seconds")
            .with_label("model", report.model.clone())
            .with_label("table", report.table.clone())
            .record(std::time::Duration::from_millis(report.duration_ms));
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
                table: "widgets".to_string(),
                rows_swept: 3,
                duration_ms: 5,
                dry_run: true,
            })
        })
    }

    fn sample_task_info() -> TaskInfo {
        task_info_named("retention-sweep-widget")
    }

    fn task_info_named(name: &str) -> TaskInfo {
        TaskInfo {
            name: name.to_string(),
            schedule: crate::task::Schedule::FixedDelay(std::time::Duration::from_secs(3600)),
            coordination: crate::task::TaskCoordination::Fleet,
            handler: |_state| Box::pin(async { Ok(()) }),
        }
    }

    // `RetentionSweepDescriptor::task_info` is a plain `fn() -> TaskInfo`
    // (no captures), so each `inventory::submit!` fixture below needs its
    // own named function rather than a closure over `task_info_named` — and
    // each must return a distinct name, now that `collect_retention_tasks`
    // rejects duplicates (#1342 review round 7). Sharing `sample_task_info`
    // across fixtures, as earlier versions of these tests did, is exactly
    // the collision that check now catches.
    fn by_model_name_task_info() -> TaskInfo {
        task_info_named("retention-sweep-__retention_runtime_test_widgets")
    }

    fn by_table_name_task_info() -> TaskInfo {
        task_info_named("retention-sweep-__retention_runtime_test_widgets_by_table")
    }

    fn ambiguous_a_task_info() -> TaskInfo {
        task_info_named("retention-sweep-__retention_runtime_ambiguous_widgets_a")
    }

    fn ambiguous_b_task_info() -> TaskInfo {
        task_info_named("retention-sweep-__retention_runtime_ambiguous_widgets_b")
    }

    #[test]
    fn report_serializes_with_expected_fields() {
        let report = RetentionSweepReport {
            model: "Widget".to_string(),
            table: "widgets".to_string(),
            rows_swept: 42,
            duration_ms: 7,
            dry_run: false,
        };

        let json = serde_json::to_value(&report).expect("report should serialize");
        assert_eq!(json["model"], "Widget");
        assert_eq!(json["table"], "widgets");
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

    #[test]
    fn duplicate_retention_task_name_detects_a_collision() {
        // Regression (#1342 review round 7): two different repositories can
        // legitimately target the same table (see live_broadcast.rs); if
        // both declare retention(...), the table-qualified task name alone
        // doesn't disambiguate them. Tested against the pure helper, not
        // collect_retention_tasks() itself — inventory::submit! is
        // process-global and can't be un-registered, so submitting a
        // colliding pair here would break every other test in the binary
        // that also calls collect_retention_tasks().
        let tasks = vec![sample_task_info(), sample_task_info()];
        let message = duplicate_retention_task_name(&tasks)
            .expect("two tasks with the same name must be flagged as a collision");
        assert!(message.contains(&sample_task_info().name), "{message}");
    }

    #[test]
    fn duplicate_retention_task_name_accepts_unique_names() {
        let mut b = sample_task_info();
        b.name = "retention-sweep-other-table".to_string();
        let tasks = vec![sample_task_info(), b];
        assert!(
            duplicate_retention_task_name(&tasks).is_none(),
            "two tasks with different names must not be flagged as a collision"
        );
    }

    #[tokio::test]
    async fn run_retention_dry_run_filters_by_model_name() {
        struct Fixture;
        inventory::submit! {
            RetentionSweepDescriptor {
                model_name: "__RetentionRuntimeTestWidget",
                table_name: "__retention_runtime_test_widgets",
                task_info: by_model_name_task_info,
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
    async fn run_retention_dry_run_filters_by_table_name() {
        struct Fixture;
        inventory::submit! {
            RetentionSweepDescriptor {
                model_name: "__RetentionRuntimeTestWidgetByTable",
                table_name: "__retention_runtime_test_widgets_by_table",
                task_info: by_table_name_task_info,
                dry_run: sample_dry_run,
            }
        }
        let _ = Fixture;

        let state = AppState::for_test();
        let reports =
            run_retention_dry_run(&state, Some("__retention_runtime_test_widgets_by_table"))
                .await
                .expect("dry run should succeed when filtering by table name");

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].model, "Widget");
    }

    #[tokio::test]
    async fn run_retention_dry_run_rejects_ambiguous_model_filter() {
        struct FixtureA;
        struct FixtureB;
        inventory::submit! {
            RetentionSweepDescriptor {
                model_name: "__RetentionRuntimeAmbiguousWidget",
                table_name: "__retention_runtime_ambiguous_widgets_a",
                task_info: ambiguous_a_task_info,
                dry_run: sample_dry_run,
            }
        }
        inventory::submit! {
            RetentionSweepDescriptor {
                model_name: "__RetentionRuntimeAmbiguousWidget",
                table_name: "__retention_runtime_ambiguous_widgets_b",
                task_info: ambiguous_b_task_info,
                dry_run: sample_dry_run,
            }
        }
        let _ = (FixtureA, FixtureB);

        let state = AppState::for_test();
        let error = run_retention_dry_run(&state, Some("__RetentionRuntimeAmbiguousWidget"))
            .await
            .expect_err("two policies sharing a model name must be rejected as ambiguous");

        assert!(
            error
                .to_string()
                .contains("__retention_runtime_ambiguous_widgets_a")
                || error
                    .to_string()
                    .contains("__retention_runtime_ambiguous_widgets_b"),
            "error should name the disambiguating table names: {error}"
        );
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
