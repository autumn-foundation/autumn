use std::time::Duration;

use autumn_web::actuator::TaskRegistry;
use autumn_web::config::{AutumnConfig, MockEnv, SchedulerBackend, SchedulerConfig};
use autumn_web::scheduler::{
    InProcessSchedulerCoordinator, SchedulerCoordinator, advisory_lock_key, fixed_delay_tick_key,
};
use autumn_web::task::TaskCoordination;

#[test]
fn scheduler_config_defaults_to_in_process_backend() {
    let config = AutumnConfig::default();

    assert_eq!(config.scheduler.backend, SchedulerBackend::InProcess);
    assert_eq!(config.scheduler.lease_ttl_secs, 300);
    assert_eq!(config.scheduler.key_prefix, "autumn:scheduler");
}

#[test]
fn scheduler_config_deserializes_postgres_backend() {
    let config: AutumnConfig = toml::from_str(
        r#"
        [scheduler]
        backend = "postgres"
        lease_ttl_secs = 45
        replica_id = "pod-a"
        key_prefix = "orders:scheduler"
        "#,
    )
    .expect("scheduler config should deserialize");

    assert_eq!(config.scheduler.backend, SchedulerBackend::Postgres);
    assert_eq!(config.scheduler.lease_ttl_secs, 45);
    assert_eq!(config.scheduler.replica_id.as_deref(), Some("pod-a"));
    assert_eq!(config.scheduler.key_prefix, "orders:scheduler");
}

#[test]
fn scheduler_config_supports_env_overrides() {
    let env = MockEnv::new()
        .with("AUTUMN_SCHEDULER__BACKEND", "postgres")
        .with("AUTUMN_SCHEDULER__LEASE_TTL_SECS", "60")
        .with("AUTUMN_SCHEDULER__REPLICA_ID", "machine-2")
        .with("AUTUMN_SCHEDULER__KEY_PREFIX", "billing:scheduler");
    let mut config = AutumnConfig::default();

    config.apply_env_overrides_with_env(&env);

    assert_eq!(config.scheduler.backend, SchedulerBackend::Postgres);
    assert_eq!(config.scheduler.lease_ttl_secs, 60);
    assert_eq!(config.scheduler.replica_id.as_deref(), Some("machine-2"));
    assert_eq!(config.scheduler.key_prefix, "billing:scheduler");
}

#[test]
fn fixed_delay_tick_key_is_stable_within_interval() {
    let first = fixed_delay_tick_key(
        "cleanup",
        Duration::from_secs(10),
        Duration::from_secs(1_700_000_004),
    );
    let second = fixed_delay_tick_key(
        "cleanup",
        Duration::from_secs(10),
        Duration::from_secs(1_700_000_009),
    );
    let next = fixed_delay_tick_key(
        "cleanup",
        Duration::from_secs(10),
        Duration::from_secs(1_700_000_010),
    );

    assert_eq!(first, second);
    assert_ne!(first, next);
    assert!(first.contains("cleanup"));
}

#[test]
fn fixed_delay_tick_key_uses_sub_second_precision() {
    let delay = Duration::from_millis(100);
    let first = fixed_delay_tick_key(
        "fast",
        delay,
        Duration::from_secs(1_700_000_000) + Duration::from_millis(100),
    );
    let same = fixed_delay_tick_key(
        "fast",
        delay,
        Duration::from_secs(1_700_000_000) + Duration::from_millis(199),
    );
    let next = fixed_delay_tick_key(
        "fast",
        delay,
        Duration::from_secs(1_700_000_000) + Duration::from_millis(200),
    );

    assert_eq!(first, same);
    assert_ne!(first, next);
}

#[test]
fn advisory_lock_key_is_deterministic_and_prefix_scoped() {
    let first = advisory_lock_key("autumn:scheduler", "cleanup", "cleanup:170000000");
    let second = advisory_lock_key("autumn:scheduler", "cleanup", "cleanup:170000000");
    let different_prefix = advisory_lock_key("other:scheduler", "cleanup", "cleanup:170000000");

    assert_eq!(first, second);
    assert_ne!(first, different_prefix);
}

#[tokio::test]
async fn in_process_coordinator_acquires_fleet_tasks_locally() {
    let coordinator = InProcessSchedulerCoordinator::new("replica-a");

    let lease = coordinator
        .try_acquire("cleanup", "cleanup:170000000", TaskCoordination::Fleet)
        .await
        .expect("in-process acquisition should not fail")
        .expect("in-process backend should acquire locally");

    assert_eq!(lease.backend(), "in_process");
    assert_eq!(lease.leader_id(), "replica-a");
}

#[tokio::test]
async fn in_process_coordinator_marks_per_replica_tasks_as_uncoordinated() {
    let coordinator = InProcessSchedulerCoordinator::new("replica-a");

    let lease = coordinator
        .try_acquire(
            "cache-warm",
            "cache-warm:170000000",
            TaskCoordination::PerReplica,
        )
        .await
        .expect("in-process acquisition should not fail")
        .expect("per-replica task should run on this replica");

    assert_eq!(lease.backend(), "per_replica");
    assert_eq!(lease.leader_id(), "replica-a");
}

#[test]
fn task_registry_exposes_scheduler_coordination_metadata() {
    let registry = TaskRegistry::new();

    registry.register_scheduled(
        "cleanup",
        "every 10s",
        TaskCoordination::Fleet,
        "postgres",
        "replica-a",
    );
    registry.record_leader("cleanup", "replica-b", "cleanup:170000000");
    registry.record_success("cleanup", 12);

    let snapshot = registry.snapshot();
    let cleanup = &snapshot["cleanup"];

    assert_eq!(cleanup.coordination, TaskCoordination::Fleet);
    assert_eq!(cleanup.scheduler_backend, "postgres");
    assert_eq!(cleanup.replica_id, "replica-a");
    assert_eq!(cleanup.current_leader.as_deref(), Some("replica-b"));
    assert_eq!(cleanup.last_tick.as_deref(), Some("cleanup:170000000"));
    assert!(cleanup.last_fired_at.is_some());
}

#[test]
fn postgres_scheduler_requires_a_database_pool() {
    let state = autumn_web::AppState::for_test();
    let config = SchedulerConfig {
        backend: SchedulerBackend::Postgres,
        ..SchedulerConfig::default()
    };

    let Err(error) = autumn_web::scheduler::coordinator_from_config(&config, &state) else {
        panic!("postgres scheduler should fail before boot without a database pool");
    };

    assert!(error.to_string().contains("postgres"));
}
