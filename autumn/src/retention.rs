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
    let descriptors = resolve_retention_descriptors(model_filter)?;
    let mut reports = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        reports.push((descriptor.dry_run)(state.clone()).await?);
    }
    reports.sort_by(|a, b| a.model.cmp(&b.model).then_with(|| a.table.cmp(&b.table)));
    Ok(reports)
}

/// Resolve `model_filter` to the descriptors [`run_retention_dry_run`] would
/// query, without touching a database — every check here reads only the
/// `inventory`-collected registry (and each matched descriptor's own
/// `task_info()`, itself DB-independent).
///
/// `None` resolves to every registered descriptor (possibly empty, if the
/// binary declares no `retention(...)` policy at all). This is what lets
/// `AppBuilder::run_retention_dry_run_mode` decide whether `autumn retention
/// --dry-run` needs a database connection at all *before* opening one — see
/// `docs/guide/retention-sweeps.md`.
///
/// Also runs `task_info()` on every registered descriptor — the same
/// duration-parsing, model-dependents, bind-limit, and duplicate-task-name
/// validation real boot runs via `collect_retention_tasks()` — so a dry run
/// is a faithful preview: a policy that would panic at boot panics here
/// too, rather than reporting a misleadingly clean dry run (#1342 review
/// round 14). This validates the *complete* registry, not just the
/// descriptors `model_filter` narrows down to (#1342 review round 19):
/// real boot has no filter concept, so a `--model`-scoped dry run must
/// still surface a collision between two other, unselected policies.
///
/// # Errors
///
/// A not-found error when `model_filter` names nothing registered, or a
/// bad-request error when it matches more than one policy.
///
/// # Panics
///
/// Panics if any registered descriptor's declared `after`,
/// `purge_deleted_after`, or `every` is not a valid duration string, if its
/// model declares a model-side `dependent = ...` association, if its
/// `batch_size` doesn't fit this backend's bind-parameter limit, or if two
/// registered descriptors produce the same generated task name — the same
/// conditions `collect_retention_tasks()` panics on at boot.
#[doc(hidden)]
pub fn resolve_retention_descriptors(
    model_filter: Option<&str>,
) -> AutumnResult<Vec<&'static RetentionSweepDescriptor>> {
    let all: Vec<&RetentionSweepDescriptor> =
        inventory::iter::<RetentionSweepDescriptor>().collect();
    // Run the same checks boot runs in `collect_retention_tasks()`: the duration, dependents
    // and bind-limit asserts inside `task_info()`, plus the duplicate-task-name check. They
    // are pure, so this still resolves before connecting. Validate `all`, not the
    // `--model`-narrowed matches: boot has no filter and panics on any bad policy (#1342).
    validate_resolved_descriptors(&all);

    let matches: Vec<&RetentionSweepDescriptor> = match model_filter {
        None => all,
        Some(filter) => {
            // A unique `table_name` match wins first. The ambiguity error tells the operator
            // to pass the table name, and the model-or-table filter alone could match it
            // twice (model `Session` in one policy, a table named `Session` in another).
            let table_matches: Vec<&RetentionSweepDescriptor> = all
                .iter()
                .copied()
                .filter(|descriptor| descriptor.table_name == filter)
                .collect();
            let found: Vec<&RetentionSweepDescriptor> = if table_matches.len() == 1 {
                table_matches
            } else {
                all.into_iter()
                    .filter(|descriptor| {
                        descriptor.model_name == filter || descriptor.table_name == filter
                    })
                    .collect()
            };
            if found.is_empty() {
                return Err(crate::AutumnError::not_found_msg(format!(
                    "no #[repository(..., retention(...))] policy is registered for model \
                     {filter:?}"
                )));
            }
            if found.len() > 1 {
                let table_names: Vec<&str> = found.iter().map(|d| d.table_name).collect();
                return Err(crate::AutumnError::bad_request_msg(format!(
                    "{filter:?} matches more than one retention policy ({table_names:?}); pass \
                     the table name instead of the model name to disambiguate, e.g. --model {}",
                    table_names[0]
                )));
            }
            found
        }
    };
    Ok(matches)
}

/// Every registered retention descriptor, unfiltered — the same set
/// [`collect_retention_tasks`] walks at boot.
///
/// Used by the dry-run mode to validate hand-declared task-name collisions
/// against the complete registry rather than whatever `--model` narrows the
/// actual dry-run count to (#1342 review round 19) — the same reasoning
/// [`resolve_retention_descriptors`] applies to its own duplicate-name
/// check.
#[doc(hidden)]
#[must_use]
pub fn all_retention_descriptors() -> Vec<&'static RetentionSweepDescriptor> {
    inventory::iter::<RetentionSweepDescriptor>().collect()
}

/// Runs each descriptor's `task_info()` — the same duration/dependents/
/// bind-limit validation `collect_retention_tasks()` runs at boot — and
/// checks the resulting task names for a collision, matching
/// `collect_retention_tasks()`'s own duplicate check.
///
/// A free function (rather than inlined into
/// [`resolve_retention_descriptors`]) so the collision case is
/// unit-testable against synthetic descriptors — testing it through
/// `resolve_retention_descriptors` itself would mean `inventory::submit!`-ing
/// a colliding pair into the process-global registry, which every other test
/// in the binary shares and can't un-register.
///
/// # Panics
///
/// See [`resolve_retention_descriptors`].
fn validate_resolved_descriptors(matches: &[&RetentionSweepDescriptor]) {
    let task_infos: Vec<TaskInfo> = matches.iter().map(|d| (d.task_info)()).collect();
    if let Some(message) = duplicate_retention_task_name(&task_infos) {
        panic!("{message}");
    }
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

    // `RetentionSweepDescriptor::task_info` is a plain `fn() -> TaskInfo`, so each
    // `inventory::submit!` fixture needs its own named function, and each must return a
    // distinct name: `collect_retention_tasks` rejects duplicates.
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

    fn table_shadows_model_task_info_a() -> TaskInfo {
        task_info_named("retention-sweep-__retention_runtime_table_shadows_model_a")
    }

    fn table_shadows_model_task_info_b() -> TaskInfo {
        task_info_named("retention-sweep-__retention_runtime_table_shadows_model_b")
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
        // Descriptors from other test modules may be registered too, so do not assert an
        // exact count.
        let tasks = collect_retention_tasks();
        assert!(
            tasks.iter().all(|t| !t.name.is_empty()),
            "every collected retention task must have a name"
        );
    }

    #[test]
    fn duplicate_retention_task_name_detects_a_collision() {
        // Two repositories can target the same table, and both may declare retention(...).
        // This tests the pure helper: a colliding pair submitted to the process-global
        // inventory would break every other test that calls collect_retention_tasks().
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

    #[test]
    fn resolve_retention_descriptors_prefers_exact_table_match_over_ambiguous_model_match() {
        // One policy's table_name equals another's model_name. The table-name filter the
        // ambiguity error suggests must still resolve to exactly one policy.
        struct FixtureA;
        struct FixtureB;
        inventory::submit! {
            RetentionSweepDescriptor {
                model_name: "__RetentionRuntimeTableShadowsModel",
                table_name: "__retention_runtime_table_shadows_model_a",
                task_info: table_shadows_model_task_info_a,
                dry_run: sample_dry_run,
            }
        }
        inventory::submit! {
            RetentionSweepDescriptor {
                model_name: "__RetentionRuntimeTableShadowsModelOther",
                table_name: "__RetentionRuntimeTableShadowsModel",
                task_info: table_shadows_model_task_info_b,
                dry_run: sample_dry_run,
            }
        }
        let _ = (FixtureA, FixtureB);

        let descriptors =
            resolve_retention_descriptors(Some("__RetentionRuntimeTableShadowsModel")).expect(
                "an exact table-name match must resolve unambiguously even though another \
                 policy's model_name equals the same string",
            );

        assert_eq!(descriptors.len(), 1);
        assert_eq!(
            descriptors[0].table_name,
            "__RetentionRuntimeTableShadowsModel"
        );
        assert_eq!(
            descriptors[0].model_name,
            "__RetentionRuntimeTableShadowsModelOther"
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

    // A dry run must run the `task_info()` validation boot runs (#1342). A panicking fixture
    // would break every other test that calls `collect_retention_tasks()`, since the
    // `inventory` registry is process-global, so count `task_info()` calls instead.
    static COUNTING_TASK_INFO_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    fn counting_task_info() -> TaskInfo {
        COUNTING_TASK_INFO_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        task_info_named("retention-sweep-__retention_runtime_test_counting")
    }

    #[test]
    fn resolve_retention_descriptors_invokes_task_info_validation() {
        struct Fixture;
        inventory::submit! {
            RetentionSweepDescriptor {
                model_name: "__RetentionRuntimeTestCounting",
                table_name: "__retention_runtime_test_counting",
                task_info: counting_task_info,
                dry_run: sample_dry_run,
            }
        }
        let _ = Fixture;

        let before = COUNTING_TASK_INFO_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        let descriptors = resolve_retention_descriptors(Some("__RetentionRuntimeTestCounting"))
            .expect("resolving a registered model must succeed");
        let after = COUNTING_TASK_INFO_CALLS.load(std::sync::atomic::Ordering::SeqCst);

        assert_eq!(descriptors.len(), 1);
        assert!(
            after > before,
            "resolve_retention_descriptors must call task_info() on every matched descriptor \
             to run the same validation collect_retention_tasks() runs at boot, so a dry run \
             cannot report success for a policy real boot would panic on"
        );
    }

    // Boot has no `--model` filter, so a filtered dry run must still call `task_info()` on
    // policies the filter does not select. This fixture is never selected.
    static UNSELECTED_TASK_INFO_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    fn unselected_counting_task_info() -> TaskInfo {
        UNSELECTED_TASK_INFO_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        task_info_named("retention-sweep-__retention_runtime_test_unselected")
    }

    fn filter_selected_task_info() -> TaskInfo {
        task_info_named("retention-sweep-__retention_runtime_test_filter_selected")
    }

    #[test]
    fn resolve_retention_descriptors_validates_the_full_registry_even_when_filtered() {
        struct SelectedFixture;
        struct UnselectedFixture;
        inventory::submit! {
            RetentionSweepDescriptor {
                model_name: "__RetentionRuntimeTestFilterSelected",
                table_name: "__retention_runtime_test_filter_selected",
                task_info: filter_selected_task_info,
                dry_run: sample_dry_run,
            }
        }
        inventory::submit! {
            RetentionSweepDescriptor {
                model_name: "__RetentionRuntimeTestFilterUnselected",
                table_name: "__retention_runtime_test_unselected",
                task_info: unselected_counting_task_info,
                dry_run: sample_dry_run,
            }
        }
        let _ = (SelectedFixture, UnselectedFixture);

        let before = UNSELECTED_TASK_INFO_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        let descriptors =
            resolve_retention_descriptors(Some("__RetentionRuntimeTestFilterSelected"))
                .expect("resolving a registered model must succeed");
        let after = UNSELECTED_TASK_INFO_CALLS.load(std::sync::atomic::Ordering::SeqCst);

        assert_eq!(
            descriptors.len(),
            1,
            "the returned/counted set must still be narrowed to the --model filter"
        );
        assert!(
            after > before,
            "resolve_retention_descriptors must validate every registered descriptor, \
             including ones --model does not select, since real boot has no filter concept"
        );
    }

    #[test]
    fn validate_resolved_descriptors_panics_on_duplicate_task_names() {
        // A dry run must reject the duplicate task names boot rejects. Synthetic descriptors,
        // not inventory::submit!: a colliding pair in the process-global registry would break
        // every other test that calls collect_retention_tasks().
        let a = RetentionSweepDescriptor {
            model_name: "__ValidateResolvedDescriptorsA",
            table_name: "__validate_resolved_descriptors_dup",
            task_info: || task_info_named("retention-sweep-__validate_resolved_descriptors_dup"),
            dry_run: sample_dry_run,
        };
        let b = RetentionSweepDescriptor {
            model_name: "__ValidateResolvedDescriptorsB",
            table_name: "__validate_resolved_descriptors_dup",
            task_info: || task_info_named("retention-sweep-__validate_resolved_descriptors_dup"),
            dry_run: sample_dry_run,
        };
        let matches: Vec<&RetentionSweepDescriptor> = vec![&a, &b];

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            validate_resolved_descriptors(&matches);
        }));

        assert!(
            result.is_err(),
            "validate_resolved_descriptors must panic when two descriptors produce the same \
             task name"
        );
    }

    #[test]
    fn validate_resolved_descriptors_accepts_unique_task_names() {
        let a = RetentionSweepDescriptor {
            model_name: "__ValidateResolvedDescriptorsUniqueA",
            table_name: "__validate_resolved_descriptors_unique_a",
            task_info: || {
                task_info_named("retention-sweep-__validate_resolved_descriptors_unique_a")
            },
            dry_run: sample_dry_run,
        };
        let b = RetentionSweepDescriptor {
            model_name: "__ValidateResolvedDescriptorsUniqueB",
            table_name: "__validate_resolved_descriptors_unique_b",
            task_info: || {
                task_info_named("retention-sweep-__validate_resolved_descriptors_unique_b")
            },
            dry_run: sample_dry_run,
        };
        let matches: Vec<&RetentionSweepDescriptor> = vec![&a, &b];

        validate_resolved_descriptors(&matches);
    }
}
