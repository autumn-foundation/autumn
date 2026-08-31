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
    // cookie's Max-Age and the Redis session TTL, the job-tracking
    // `expires_at`).
    let mut config = AutumnConfig::default();
    config.jobs.tracking.ttl_secs = 86_400;
    config.idempotency.ttl_secs = 86_400;
    config.session.max_age_secs = 86_400;
    config.retention.job_tracking = Some("1h".to_owned());
    config.retention.idempotency = Some("2h".to_owned());
    config.retention.sessions = Some("3h".to_owned());

    config.apply_retention_caps();

    assert_eq!(config.jobs.tracking.ttl_secs, 3_600);
    assert_eq!(config.idempotency.ttl_secs, 7_200);
    assert_eq!(config.session.max_age_secs, 10_800);
}

#[test]
fn retention_caps_never_extend_a_shorter_subsystem_ttl() {
    let mut config = AutumnConfig::default();
    config.jobs.tracking.ttl_secs = 600;
    config.retention.job_tracking = Some("30d".to_owned());

    config.apply_retention_caps();

    assert_eq!(
        config.jobs.tracking.ttl_secs, 600,
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
