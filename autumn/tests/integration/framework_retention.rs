//! Unified data-retention policy for framework-owned data (issue #1605).
//!
//! Backend-independent tests over the policy surface: the dataset registry,
//! the effective-window/precedence rules against the pre-existing
//! `jobs.tracking.ttl_secs` / `idempotency.ttl_secs` knobs, the GDPR
//! legal-hold veto, and the scheduled-task registration.
//!
//! The Postgres sweeps themselves live in `framework_retention_pg.rs`
//! (Docker/testcontainers).

use std::time::Duration;

use autumn_web::config::AutumnConfig;
use autumn_web::data_retention::{
    RETENTION_DATASETS, RetentionDataset, RetentionEnforcement, RetentionSource,
    effective_retention, framework_retention_task, legal_hold_for,
};
use autumn_web::gdpr::{GdprRegistry, ModelRegistration};

// ── Dataset registry ─────────────────────────────────────────────────────

#[test]
fn every_dataset_named_by_the_issue_is_registered() {
    // AC #1's "at minimum" list, verbatim.
    let keys: Vec<&str> = RETENTION_DATASETS.iter().map(|d| d.key()).collect();
    for expected in [
        "job_history",
        "job_tracking",
        "idempotency",
        "experiment_assignments",
        "webhook_replay",
        "sessions",
        "audit_archives",
        // Beyond the issue's "at minimum" list: the `#[after_commit]` hook
        // queue is structurally identical to job history and accumulates the
        // same way.
        "commit_hooks",
    ] {
        assert!(
            keys.contains(&expected),
            "dataset {expected:?} must be in the registry, got {keys:?}"
        );
    }
}

#[test]
fn the_registry_and_the_config_surface_cannot_drift() {
    // The one structural guard against adding a `[retention]` key with no
    // sweeper behind it (or a sweeper with no way to configure it).
    let config = AutumnConfig::default();
    let config_keys: Vec<&str> = config
        .retention
        .windows()
        .iter()
        .map(|(key, _)| *key)
        .collect();
    let registry_keys: Vec<&str> = RETENTION_DATASETS.iter().map(|d| d.key()).collect();
    assert_eq!(
        config_keys, registry_keys,
        "[retention] config keys and the dataset registry must match, in order"
    );
}

#[test]
fn every_dataset_key_round_trips_and_has_documentation() {
    for dataset in RETENTION_DATASETS {
        assert_eq!(
            RetentionDataset::from_key(dataset.key()),
            Some(dataset),
            "{} must resolve from its own key",
            dataset.key()
        );
        assert!(
            !dataset.description().is_empty(),
            "{} needs a human-readable description for the CLI report",
            dataset.key()
        );
        assert!(
            !dataset.default_behavior().is_empty(),
            "{} needs a documented default ('forever' where applicable)",
            dataset.key()
        );
    }
}

#[test]
fn from_key_rejects_an_unknown_dataset() {
    assert_eq!(RetentionDataset::from_key("not_a_dataset"), None);
}

#[test]
fn sweep_enforced_datasets_name_their_backing_table() {
    for dataset in RETENTION_DATASETS {
        if dataset.enforcement() == RetentionEnforcement::Sweep {
            assert!(
                dataset.table().is_some(),
                "{} is sweep-enforced so it must name the table it sweeps",
                dataset.key()
            );
        }
    }
}

// ── Effective window + precedence (AC #3) ────────────────────────────────

#[test]
fn an_unset_window_leaves_a_dataset_untouched() {
    let config = AutumnConfig::default();
    let effective = effective_retention(&config, RetentionDataset::JobHistory);
    assert_eq!(effective.window, None);
    assert_eq!(effective.source, RetentionSource::Unset);
}

#[test]
fn a_configured_window_is_used_when_no_subsystem_ttl_competes() {
    let mut config = AutumnConfig::default();
    config.retention.job_history = Some("90d".to_owned());
    let effective = effective_retention(&config, RetentionDataset::JobHistory);
    assert_eq!(effective.window, Some(Duration::from_secs(90 * 86_400)));
    assert_eq!(effective.source, RetentionSource::Policy);
}

#[test]
fn job_tracking_ttl_secs_still_applies_with_no_policy_window() {
    // AC #3: the existing knob keeps working unchanged.
    let mut config = AutumnConfig::default();
    config.jobs.tracking.ttl_secs = 3_600;
    let effective = effective_retention(&config, RetentionDataset::JobTracking);
    assert_eq!(effective.window, Some(Duration::from_secs(3_600)));
    assert_eq!(
        effective.source,
        RetentionSource::SubsystemTtl("jobs.tracking.ttl_secs")
    );
}

#[test]
fn the_shorter_of_policy_and_job_tracking_ttl_wins() {
    let mut config = AutumnConfig::default();
    config.jobs.tracking.ttl_secs = 86_400;

    config.retention.job_tracking = Some("1h".to_owned());
    let tighter = effective_retention(&config, RetentionDataset::JobTracking);
    assert_eq!(tighter.window, Some(Duration::from_secs(3_600)));
    assert_eq!(tighter.source, RetentionSource::Policy);

    config.retention.job_tracking = Some("30d".to_owned());
    let looser = effective_retention(&config, RetentionDataset::JobTracking);
    assert_eq!(
        looser.window,
        Some(Duration::from_secs(86_400)),
        "a policy window longer than the subsystem TTL cannot extend retention"
    );
    assert_eq!(
        looser.source,
        RetentionSource::SubsystemTtl("jobs.tracking.ttl_secs")
    );
}

#[test]
fn the_shorter_of_policy_and_idempotency_ttl_wins() {
    let mut config = AutumnConfig::default();
    config.idempotency.ttl_secs = 86_400;

    config.retention.idempotency = Some("2h".to_owned());
    let tighter = effective_retention(&config, RetentionDataset::Idempotency);
    assert_eq!(tighter.window, Some(Duration::from_secs(7_200)));
    assert_eq!(tighter.source, RetentionSource::Policy);

    config.retention.idempotency = Some("7d".to_owned());
    let looser = effective_retention(&config, RetentionDataset::Idempotency);
    assert_eq!(looser.window, Some(Duration::from_secs(86_400)));
    assert_eq!(
        looser.source,
        RetentionSource::SubsystemTtl("idempotency.ttl_secs")
    );
}

#[test]
fn sessions_fall_back_to_the_cookie_max_age() {
    let mut config = AutumnConfig::default();
    config.session.max_age_secs = 7_200;
    let effective = effective_retention(&config, RetentionDataset::Sessions);
    assert_eq!(effective.window, Some(Duration::from_secs(7_200)));
    assert_eq!(
        effective.source,
        RetentionSource::SubsystemTtl("session.max_age_secs")
    );
}

#[test]
fn audit_archives_default_to_forever() {
    let config = AutumnConfig::default();
    let effective = effective_retention(&config, RetentionDataset::AuditArchives);
    assert_eq!(
        effective.window, None,
        "audit archives are kept forever until a window is declared"
    );
    assert_eq!(
        RetentionDataset::AuditArchives.default_behavior(),
        "forever"
    );
}

#[test]
fn experiment_assignments_default_to_forever() {
    let config = AutumnConfig::default();
    assert_eq!(
        effective_retention(&config, RetentionDataset::ExperimentAssignments).window,
        None
    );
}

// ── Legal hold (AC #5) ───────────────────────────────────────────────────

#[test]
fn a_gdpr_retain_registration_blocks_the_dataset_that_backs_it() {
    let registry = GdprRegistry::new().register(ModelRegistration::retain(
        "autumn_jobs",
        "SOX: job history retained 7 years",
    ));
    let hold = legal_hold_for(RetentionDataset::JobHistory, Some(&registry))
        .expect("a retain registration on autumn_jobs must place job_history on hold");
    assert!(
        hold.contains("SOX"),
        "the hold reason must be surfaced verbatim: {hold}"
    );
}

#[test]
fn a_hard_delete_or_anonymize_registration_is_not_a_legal_hold() {
    let registry = GdprRegistry::new()
        .register(ModelRegistration::hard_delete("autumn_jobs"))
        .register(ModelRegistration::anonymize(
            "autumn_experiment_assignments",
        ));
    assert_eq!(
        legal_hold_for(RetentionDataset::JobHistory, Some(&registry)),
        None
    );
    assert_eq!(
        legal_hold_for(RetentionDataset::ExperimentAssignments, Some(&registry)),
        None
    );
}

#[test]
fn a_hold_on_one_table_does_not_block_a_different_dataset() {
    let registry =
        GdprRegistry::new().register(ModelRegistration::retain("autumn_jobs", "litigation hold"));
    assert_eq!(
        legal_hold_for(RetentionDataset::ExperimentAssignments, Some(&registry)),
        None
    );
}

#[test]
fn no_registry_means_no_hold() {
    assert_eq!(legal_hold_for(RetentionDataset::JobHistory, None), None);
}

// ── Scheduled task registration (AC #2) ──────────────────────────────────

#[test]
fn no_window_registers_no_sweep_task() {
    // AC #1: an app that never mentions [retention] must be bit-for-bit
    // unchanged — including having no extra scheduler loop.
    let config = AutumnConfig::default();
    assert!(framework_retention_task(&config.retention).is_none());
}

#[test]
fn one_window_registers_a_fleet_coordinated_recurring_task() {
    let mut config = AutumnConfig::default();
    config.retention.job_history = Some("90d".to_owned());
    config.retention.sweep_interval = "30m".to_owned();

    let task = framework_retention_task(&config.retention)
        .expect("a configured window must register the sweep");
    assert_eq!(task.name, "autumn-retention-sweep");
    assert_eq!(
        task.coordination,
        autumn_web::task::TaskCoordination::Fleet,
        "one replica per tick, not one delete storm per replica"
    );
    match task.schedule {
        autumn_web::task::Schedule::FixedDelay(delay) => {
            assert_eq!(delay, Duration::from_secs(1_800));
        }
        autumn_web::task::Schedule::Cron { .. } => {
            panic!("the framework sweep runs on a fixed delay, not cron")
        }
        _ => panic!("unexpected schedule variant"),
    }
}

// ── Runtime enforcement of TTL-native datasets (AC #2/#3) ────────────────

#[test]
fn retention_caps_shorten_subsystem_ttls_in_place() {
    // The TTL-native datasets are enforced by writing records with a shorter
    // TTL, so the cap is applied once to the loaded config and then flows
    // into every derived lifetime (the idempotency layer's TTL, the session
    // cookie's Max-Age, the Redis session TTL).
    let mut config = AutumnConfig::default();
    config.idempotency.ttl_secs = 86_400;
    config.session.max_age_secs = 86_400;
    config.retention.idempotency = Some("2h".to_owned());
    config.retention.sessions = Some("3h".to_owned());

    config.apply_retention_caps();

    assert_eq!(config.idempotency.ttl_secs, 7_200);
    assert_eq!(config.session.max_age_secs, 10_800);
}

#[test]
fn retention_caps_leave_job_tracking_ttl_alone() {
    // `job_tracking` is sweep-enforced, and the sweep honours a legal hold on
    // `autumn_job_tracking` while `job.rs`'s independent `expires_at` cleanup
    // cannot. Capping `jobs.tracking.ttl_secs` would let that cleanup delete,
    // on exactly the retention schedule, the rows the retention report says
    // are being preserved under hold.
    let mut config = AutumnConfig::default();
    config.jobs.backend = "postgres".to_owned();
    config.jobs.tracking.ttl_secs = 86_400;
    config.retention.job_tracking = Some("1h".to_owned());

    config.apply_retention_caps();

    assert_eq!(
        config.jobs.tracking.ttl_secs, 86_400,
        "the sweep, not the TTL, enforces job_tracking retention on postgres"
    );
}

#[test]
fn retention_caps_never_extend_a_shorter_subsystem_ttl() {
    let mut config = AutumnConfig::default();
    config.idempotency.ttl_secs = 600;
    config.retention.idempotency = Some("30d".to_owned());

    config.apply_retention_caps();

    assert_eq!(
        config.idempotency.ttl_secs, 600,
        "a longer policy window must not extend a tighter subsystem TTL"
    );
}

#[test]
fn retention_caps_are_a_no_op_without_a_policy() {
    // AC #3: the existing knobs keep working *unchanged* when no unified
    // window is declared.
    let mut config = AutumnConfig::default();
    config.jobs.tracking.ttl_secs = 12_345;
    config.idempotency.ttl_secs = 23_456;
    config.session.max_age_secs = 34_567;

    config.apply_retention_caps();

    assert_eq!(config.jobs.tracking.ttl_secs, 12_345);
    assert_eq!(config.idempotency.ttl_secs, 23_456);
    assert_eq!(config.session.max_age_secs, 34_567);
}

#[test]
fn retention_caps_are_idempotent() {
    let mut config = AutumnConfig::default();
    config.idempotency.ttl_secs = 86_400;
    config.retention.idempotency = Some("2h".to_owned());

    config.apply_retention_caps();
    config.apply_retention_caps();

    assert_eq!(config.idempotency.ttl_secs, 7_200);
}

// ── The docs page must list every dataset (AC #7) ────────────────────────

#[test]
fn the_docs_page_enumerates_every_registered_dataset() {
    // AC #7 asks the guide to enumerate *every* framework-owned dataset. The
    // table is hand-written prose, so without this guard adding a dataset
    // silently leaves the page — the thing an operator actually reads to
    // answer "how long do you keep this?" — incomplete.
    let guide = include_str!("../../../docs/guide/data-retention.md");
    for dataset in RETENTION_DATASETS {
        assert!(
            guide.contains(dataset.key()),
            "docs/guide/data-retention.md must document the {:?} dataset",
            dataset.key()
        );
        assert!(
            guide.contains(dataset.default_behavior()),
            "docs/guide/data-retention.md must state {:?}'s default retention ({:?})",
            dataset.key(),
            dataset.default_behavior()
        );
    }
}

#[test]
fn a_webhook_replay_window_shorter_than_replay_protection_is_rejected() {
    // A retention window is a *compliance* knob; letting it silently shorten
    // the replay-rejection window would weaken a *security* control through
    // a door nobody would think to look behind. Fail closed and say which
    // knob to lower instead.
    let config: AutumnConfig = toml::from_str(
        r#"
        [retention]
        webhook_replay = "1h"

        [[security.webhooks.endpoints]]
        name = "stripe"
        path = "/webhooks/stripe"
        provider = "stripe"
        secret = "whsec_test_secret_value_long_enough"
        replay_window_secs = 86400
        "#,
    )
    .expect("the TOML itself is well-formed");

    let error = config
        .validate()
        .expect_err("a retention window under the replay window must fail boot");
    let message = error.to_string();
    assert!(message.contains("retention.webhook_replay"), "{message}");
    assert!(message.contains("replay_window_secs"), "{message}");
}

#[test]
fn a_webhook_replay_window_at_or_above_replay_protection_is_accepted() {
    let config: AutumnConfig = toml::from_str(
        r#"
        [retention]
        webhook_replay = "7d"

        [[security.webhooks.endpoints]]
        name = "stripe"
        path = "/webhooks/stripe"
        provider = "stripe"
        secret = "whsec_test_secret_value_long_enough"
        replay_window_secs = 86400
        "#,
    )
    .expect("well-formed");
    config.validate().expect("a wider window is fine");
}

// ── The scheduled handler actually sweeps (AC #2) ────────────────────────

#[tokio::test]
async fn the_registered_task_handler_runs_a_real_sweep() {
    // `framework_retention_task` is the whole of "automatically on a
    // recurring schedule inside the running app". Asserting only its name and
    // cadence would leave the handler itself — the part that does the work —
    // never executed by any test.
    let mut config = AutumnConfig::default();
    config.retention.audit_archives = Some("1h".to_owned());

    let task = framework_retention_task(&config.retention).expect("a window registers the task");
    let state = autumn_web::AppState::for_test();
    state.insert_extension(config);

    (task.handler)(state)
        .await
        .expect("the sweep handler must run to completion with no database configured");
}

#[tokio::test]
async fn a_sweep_with_no_database_reports_the_reason_instead_of_failing() {
    // A `db`-less app (or one that simply has no pool) must report why a
    // sweep-enforced dataset was skipped, not error the whole run.
    let mut config = AutumnConfig::default();
    config.retention.job_history = Some("30d".to_owned());
    let state = autumn_web::AppState::for_test();
    state.insert_extension(config);

    let reports = autumn_web::data_retention::run_retention(
        &state,
        &autumn_web::data_retention::RetentionRunOptions {
            dry_run: true,
            dataset: Some("job_history"),
        },
    )
    .await
    .expect("a pool-less run is not an error");

    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].rows_removed, 0);
    assert!(
        reports[0].skipped.is_some(),
        "the operator must be told why nothing was swept: {:?}",
        reports[0]
    );
    assert_eq!(reports[0].error, None);
}

#[tokio::test]
async fn an_unknown_dataset_filter_is_rejected_rather_than_sweeping_nothing() {
    let state = autumn_web::AppState::for_test();
    let error = autumn_web::data_retention::run_retention(
        &state,
        &autumn_web::data_retention::RetentionRunOptions {
            dry_run: true,
            dataset: Some("jobs"),
        },
    )
    .await
    .expect_err("a typo must fail loudly");
    assert!(error.to_string().contains("job_history"), "{error}");
}

// ── audit_archives end to end through the engine (AC #1/#2/#6) ───────────

/// Write a JSONL audit archive whose entries carry the given ages in days.
async fn seed_archive(path: &std::path::Path, ages_in_days: &[i64]) {
    let sink = autumn_web::audit::JsonlFileAuditSink::new(path);
    for (index, age) in ages_in_days.iter().enumerate() {
        let mut event = autumn_web::audit::AuditEvent::new(
            format!("actor-{index}"),
            "auth.login",
            format!("session-{index}"),
            None,
            autumn_web::audit::AuditStatus::Success,
        );
        event.timestamp = chrono::Utc::now() - chrono::Duration::days(*age);
        autumn_web::audit::AuditSink::write(&sink, event)
            .await
            .expect("seed archive line");
    }
}

fn archive_lines(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .expect("read archive")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

#[tokio::test]
async fn the_audit_archives_dataset_purges_through_the_engine() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("audit.jsonl");
    seed_archive(&path, &[400, 400, 1]).await;

    let mut config = AutumnConfig::default();
    config.retention.audit_archives = Some("30d".to_owned());
    let state = autumn_web::AppState::for_test();
    state.insert_extension(config);
    state.insert_extension(
        autumn_web::audit::AuditLogger::new().with_sink(std::sync::Arc::new(
            autumn_web::audit::JsonlFileAuditSink::new(&path),
        )),
    );

    let reports = autumn_web::data_retention::run_retention(
        &state,
        &autumn_web::data_retention::RetentionRunOptions {
            dry_run: false,
            dataset: Some("audit_archives"),
        },
    )
    .await
    .expect("archive purge");

    assert_eq!(reports[0].rows_removed, 2, "{:?}", reports[0]);
    assert_eq!(reports[0].eligible_rows, Some(2));
    assert_eq!(reports[0].error, None);
    assert_eq!(reports[0].skipped, None);
    // The surviving entry, plus the sweep's own audit record, which lands in
    // this same archive.
    assert_eq!(archive_lines(&path), 2);
}

#[tokio::test]
async fn an_audit_archives_dry_run_counts_without_rewriting() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("audit.jsonl");
    seed_archive(&path, &[400, 400]).await;

    let mut config = AutumnConfig::default();
    config.retention.audit_archives = Some("30d".to_owned());
    let state = autumn_web::AppState::for_test();
    state.insert_extension(config);
    state.insert_extension(
        autumn_web::audit::AuditLogger::new().with_sink(std::sync::Arc::new(
            autumn_web::audit::JsonlFileAuditSink::new(&path),
        )),
    );

    let reports = autumn_web::data_retention::run_retention(
        &state,
        &autumn_web::data_retention::RetentionRunOptions {
            dry_run: true,
            dataset: Some("audit_archives"),
        },
    )
    .await
    .expect("dry run");

    assert_eq!(reports[0].eligible_rows, Some(2));
    assert_eq!(reports[0].rows_removed, 0);
    assert_eq!(
        archive_lines(&path),
        2,
        "a dry run must neither delete an entry nor append an audit record"
    );
}

#[tokio::test]
async fn audit_archives_without_a_purgeable_sink_says_so() {
    let mut config = AutumnConfig::default();
    config.retention.audit_archives = Some("30d".to_owned());
    let state = autumn_web::AppState::for_test();
    state.insert_extension(config);
    state.insert_extension(
        autumn_web::audit::AuditLogger::new()
            .with_sink(std::sync::Arc::new(autumn_web::audit::TracingAuditSink)),
    );

    let reports = autumn_web::data_retention::run_retention(
        &state,
        &autumn_web::data_retention::RetentionRunOptions {
            dry_run: true,
            dataset: Some("audit_archives"),
        },
    )
    .await
    .expect("run");

    assert!(
        reports[0]
            .skipped
            .as_deref()
            .is_some_and(|reason| reason.contains("supports purging")),
        "a forwarding-only sink must say so rather than imply an empty archive: {:?}",
        reports[0]
    );
}

#[tokio::test]
async fn a_partial_archive_purge_reports_both_the_removals_and_the_failure() {
    // Regression (#1605 Codex round 1): a purge across several sinks can
    // partly succeed. The entries the working sink deleted are gone, so the
    // report must carry that count *alongside* the error rather than
    // recording `rows_removed = 0` for data that is genuinely deleted.
    struct UnpurgeableSink;

    impl autumn_web::audit::AuditSink for UnpurgeableSink {
        fn write(
            &self,
            _event: autumn_web::audit::AuditEvent,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), autumn_web::audit::AuditError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async { Ok(()) })
        }

        fn purge_before(
            &self,
            _cutoff: chrono::DateTime<chrono::Utc>,
            _dry_run: bool,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            autumn_web::audit::AuditPurgeOutcome,
                            autumn_web::audit::AuditError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async { Err(autumn_web::audit::AuditError::new("sink offline")) })
        }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("audit.jsonl");
    seed_archive(&path, &[400, 400]).await;

    let mut config = AutumnConfig::default();
    config.retention.audit_archives = Some("30d".to_owned());
    let state = autumn_web::AppState::for_test();
    state.insert_extension(config);
    state.insert_extension(
        autumn_web::audit::AuditLogger::new()
            .with_sink(std::sync::Arc::new(
                autumn_web::audit::JsonlFileAuditSink::new(&path),
            ))
            .with_sink(std::sync::Arc::new(UnpurgeableSink)),
    );

    let reports = autumn_web::data_retention::run_retention(
        &state,
        &autumn_web::data_retention::RetentionRunOptions {
            dry_run: false,
            dataset: Some("audit_archives"),
        },
    )
    .await
    .expect("run");

    assert_eq!(
        reports[0].rows_removed, 2,
        "removals that really happened must be reported: {:?}",
        reports[0]
    );
    assert!(
        reports[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("sink offline")),
        "the failure must be reported too: {:?}",
        reports[0]
    );
}

// ── job_tracking follows the configured jobs backend (Codex round 3) ─────

#[test]
fn job_tracking_is_swept_only_when_its_records_live_in_postgres() {
    // Tracked-job records live wherever `jobs.backend` puts them. Reporting
    // `sweep` for a Redis deployment would name a Postgres table that holds
    // none of them, and claim a policy nothing enforces.
    let mut config = AutumnConfig::default();

    config.jobs.backend = "postgres".to_owned();
    assert_eq!(
        RetentionDataset::JobTracking.enforcement_for(&config),
        RetentionEnforcement::Sweep
    );

    for backend in ["redis", "local"] {
        config.jobs.backend = backend.to_owned();
        assert_eq!(
            RetentionDataset::JobTracking.enforcement_for(&config),
            RetentionEnforcement::BackendTtl,
            "{backend} keeps tracking records outside autumn_job_tracking"
        );
    }
}

#[test]
fn every_other_dataset_ignores_the_jobs_backend() {
    let mut postgres = AutumnConfig::default();
    postgres.jobs.backend = "postgres".to_owned();
    let mut redis = AutumnConfig::default();
    redis.jobs.backend = "redis".to_owned();

    for dataset in RETENTION_DATASETS {
        if dataset == RetentionDataset::JobTracking {
            continue;
        }
        assert_eq!(
            dataset.enforcement_for(&postgres),
            dataset.enforcement_for(&redis),
            "{} must not depend on jobs.backend",
            dataset.key()
        );
        assert_eq!(dataset.enforcement_for(&postgres), dataset.enforcement());
    }
}

#[test]
fn job_tracking_ttl_is_capped_only_when_no_sweep_will_enforce_it() {
    // Under `postgres` the sweep enforces the window and a legal hold can
    // stop it, so capping `jobs.tracking.ttl_secs` would let the job runner's
    // independent `expires_at` cleanup delete held rows anyway. Under any
    // other backend the record's TTL is the only bound there is, so leaving
    // it uncapped would claim a window nothing enforces.
    let mut pg = AutumnConfig::default();
    pg.jobs.backend = "postgres".to_owned();
    pg.jobs.tracking.ttl_secs = 86_400;
    pg.retention.job_tracking = Some("1h".to_owned());
    pg.apply_retention_caps();
    assert_eq!(
        pg.jobs.tracking.ttl_secs, 86_400,
        "the sweep, not the TTL, enforces job_tracking on postgres"
    );

    let mut redis = AutumnConfig::default();
    redis.jobs.backend = "redis".to_owned();
    redis.jobs.tracking.ttl_secs = 86_400;
    redis.retention.job_tracking = Some("1h".to_owned());
    redis.apply_retention_caps();
    assert_eq!(
        redis.jobs.tracking.ttl_secs, 3_600,
        "with no table to sweep, the TTL is the only bound and must be capped"
    );
}
