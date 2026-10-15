//! Postgres-backed integration tests for `PgExperimentStore`.
//!
//! **Requires Docker** to be running.

#![cfg(feature = "db")]

use std::sync::Arc;
use std::time::Duration;

use autumn_web::experiments::pg::PgExperimentStore;
use autumn_web::experiments::{
    Assignment, ExperimentConfig, ExperimentState, ExperimentStore, VariantConfig,
};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const MIGRATION_SQL: &str =
    include_str!("../../migrations/20260530300000_create_experiments/up.sql");

async fn setup_pg_store() -> (
    PgExperimentStore,
    String,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");

    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    // Run the migration on a synchronous connection.
    let mut conn = PgConnection::establish(&url).expect("db connection");
    conn.batch_execute(MIGRATION_SQL).expect("migration");

    let store = PgExperimentStore::with_cache_ttl(&url, Duration::ZERO);
    (store, url, container)
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_upsert_and_assign() {
    let (store, _url, _c) = setup_pg_store().await;
    let config = ExperimentConfig {
        name: "test_exp".to_string(),
        description: Some("my test exp".to_string()),
        state: ExperimentState::Running,
        variants: vec![
            VariantConfig {
                name: "control".to_string(),
                weight: 50,
            },
            VariantConfig {
                name: "treatment".to_string(),
                weight: 50,
            },
        ],
        winner: None,
        exclusion_group: None,
        updated_at_secs: 0,
    };

    store.upsert(config).unwrap();

    let fetched = store.get("test_exp").unwrap().unwrap();
    assert_eq!(fetched.name, "test_exp");
    assert_eq!(fetched.variants.len(), 2);

    // Record an assignment
    let assignment = Assignment {
        experiment: "test_exp".to_string(),
        actor: "user_1".to_string(),
        variant: "treatment".to_string(),
        is_override: false,
        assigned_at_secs: 0,
    };
    store.record_assignment(assignment).unwrap();

    let retrieved = store.get_assignment("test_exp", "user_1").unwrap().unwrap();
    assert_eq!(retrieved.variant, "treatment");
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_rejects_deleting_variant_with_active_assignments() {
    let (store, _url, _c) = setup_pg_store().await;
    let config = ExperimentConfig {
        name: "test_exp".to_string(),
        description: None,
        state: ExperimentState::Running,
        variants: vec![
            VariantConfig {
                name: "control".to_string(),
                weight: 50,
            },
            VariantConfig {
                name: "treatment".to_string(),
                weight: 50,
            },
        ],
        winner: None,
        exclusion_group: None,
        updated_at_secs: 0,
    };

    store.upsert(config.clone()).unwrap();

    // Record an assignment for "treatment"
    let assignment = Assignment {
        experiment: "test_exp".to_string(),
        actor: "user_1".to_string(),
        variant: "treatment".to_string(),
        is_override: false,
        assigned_at_secs: 0,
    };
    store.record_assignment(assignment).unwrap();

    // Now attempt to upsert, deleting "treatment"
    let mut new_config = config;
    new_config.variants = vec![VariantConfig {
        name: "control".to_string(),
        weight: 100,
    }];

    let res = store.upsert(new_config);
    assert!(
        res.is_err(),
        "expected upsert to fail because 'treatment' has active assignments"
    );
    let err_msg = res.unwrap_err().to_string();
    assert!(
        err_msg.contains("treatment") || err_msg.contains("active assignments"),
        "expected error to mention treatment, got: {err_msg}"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_query_level_variant_deletion_check_atomic() {
    let (store, url, _c) = setup_pg_store().await;
    let config = ExperimentConfig {
        name: "test_exp".to_string(),
        description: None,
        state: ExperimentState::Running,
        variants: vec![
            VariantConfig {
                name: "control".to_string(),
                weight: 50,
            },
            VariantConfig {
                name: "treatment".to_string(),
                weight: 50,
            },
        ],
        winner: None,
        exclusion_group: None,
        updated_at_secs: 0,
    };

    store.upsert(config).unwrap();

    // Record an assignment for "treatment"
    let assignment = Assignment {
        experiment: "test_exp".to_string(),
        actor: "user_1".to_string(),
        variant: "treatment".to_string(),
        is_override: false,
        assigned_at_secs: 0,
    };
    store.record_assignment(assignment).unwrap();

    // Now execute the SQL query directly, bypassing the Rust-level pre-check.
    // We try to upsert deleting "treatment", which should affect 0 rows.
    let mut conn = PgConnection::establish(&url).unwrap();
    let state_str = ExperimentState::Running.to_string();
    let new_variants = vec![VariantConfig {
        name: "control".to_string(),
        weight: 100,
    }];
    let variants_json = serde_json::to_string(&new_variants).unwrap();

    let rows_affected = diesel::sql_query(
        "WITH upserted AS ( \
             INSERT INTO autumn_experiments \
                 (name, description, state, variants, winner, exclusion_group) \
             VALUES ($1, $2, $3::autumn_experiment_state, $4::jsonb, $5, $6) \
             ON CONFLICT (name) DO UPDATE SET \
                 description = EXCLUDED.description, \
                 state = EXCLUDED.state, \
                 variants = EXCLUDED.variants, \
                 winner = EXCLUDED.winner, \
                 exclusion_group = EXCLUDED.exclusion_group, \
                 updated_at = NOW() \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM autumn_experiment_assignments a \
                 WHERE a.experiment = EXCLUDED.name \
                   AND a.variant NOT IN ( \
                       SELECT x.name FROM jsonb_to_recordset(EXCLUDED.variants) AS x(name text) \
                   ) \
             ) \
             RETURNING name, (xmax = 0) AS is_insert \
         ) \
         INSERT INTO autumn_experiment_changes (experiment, mutation, actor) \
         SELECT name, CASE WHEN is_insert THEN 'created' ELSE 'updated' END, NULL FROM upserted",
    )
    .bind::<diesel::sql_types::Text, _>("test_exp")
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(None::<String>)
    .bind::<diesel::sql_types::Text, _>(&state_str)
    .bind::<diesel::sql_types::Text, _>(&variants_json)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(None::<String>)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(None::<String>)
    .execute(&mut conn)
    .unwrap();

    assert_eq!(
        rows_affected, 0,
        "Expected 0 rows affected because the UPDATE check should have failed"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_set_variants_rejects_deleting_variant_with_active_assignments() {
    let (store, _url, _c) = setup_pg_store().await;
    let config = ExperimentConfig {
        name: "test_exp".to_string(),
        description: None,
        state: ExperimentState::Running,
        variants: vec![
            VariantConfig {
                name: "control".to_string(),
                weight: 50,
            },
            VariantConfig {
                name: "treatment".to_string(),
                weight: 50,
            },
        ],
        winner: None,
        exclusion_group: None,
        updated_at_secs: 0,
    };

    store.upsert(config).unwrap();

    // Record an assignment for "treatment"
    let assignment = Assignment {
        experiment: "test_exp".to_string(),
        actor: "user_1".to_string(),
        variant: "treatment".to_string(),
        is_override: false,
        assigned_at_secs: 0,
    };
    store.record_assignment(assignment).unwrap();

    // Now attempt to set_variants, deleting "treatment"
    let new_variants = vec![VariantConfig {
        name: "control".to_string(),
        weight: 100,
    }];

    let res = store.set_variants("test_exp", new_variants, None);
    assert!(
        res.is_err(),
        "expected set_variants to fail because 'treatment' has active assignments"
    );
    let err_msg = res.unwrap_err().to_string();
    assert!(
        err_msg.contains("treatment") || err_msg.contains("active assignments"),
        "expected error to mention treatment, got: {err_msg}"
    );
}

fn running(name: &str) -> ExperimentConfig {
    let mut config = ExperimentConfig::new(
        name,
        vec![
            VariantConfig::new("control", 50),
            VariantConfig::new("treatment", 50),
        ],
    );
    config.state = ExperimentState::Running;
    config
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_serves_last_known_config_when_the_database_stops() {
    let (store, _url, container) = setup_pg_store().await;
    store.upsert(running("exp")).unwrap();
    assert!(store.get("exp").unwrap().is_some());

    container.stop().await.expect("stop postgres");
    // A refresh now fails. The snapshot keeps the config as it was.
    assert!(store.refresh().is_err());
    assert!(store.refresh_errors() >= 1);
    let kept = store.get("exp").unwrap().expect("last-known config");
    assert_eq!(kept.state, ExperimentState::Running);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_sees_a_remote_write_after_refresh() {
    let (writer, _url, _c) = setup_pg_store().await;
    let reader = writer.clone(); // A clone has its own snapshot, like a replica.
    reader.refresh().unwrap();
    assert!(reader.get("remote").unwrap().is_none());

    writer.upsert(running("remote")).unwrap();
    reader.refresh().unwrap();
    assert!(reader.get("remote").unwrap().is_some());
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_writes_reach_the_next_get() {
    let (store, _url, _c) = setup_pg_store().await;
    store.upsert(running("exp")).unwrap();

    store
        .set_variants("exp", vec![VariantConfig::new("control", 100)], Some("ops"))
        .unwrap();
    assert_eq!(store.get("exp").unwrap().unwrap().variants.len(), 1);

    store
        .set_state("exp", ExperimentState::Concluded, Some("control"))
        .unwrap();
    let concluded = store.get("exp").unwrap().unwrap();
    assert_eq!(concluded.state, ExperimentState::Concluded);
    assert_eq!(concluded.winner.as_deref(), Some("control"));

    store.set_override("exp", "user_1", "control").unwrap();
    assert_eq!(
        store.get_override("exp", "user_1").unwrap().as_deref(),
        Some("control")
    );
    assert_eq!(store.list().unwrap().len(), 1);
    assert!(!store.history("exp", 10).unwrap().is_empty());
    assert!(
        !store
            .has_assignment_in_group("user_1", "none", "exp")
            .unwrap()
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_reloads_on_a_new_connection_when_the_kept_one_died() {
    let (store, url, _c) = setup_pg_store().await;
    store.refresh().unwrap(); // Keeps the refresh connection.

    let mut admin = PgConnection::establish(&url).expect("admin connection");
    admin
        .batch_execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE pid <> pg_backend_pid() AND datname = current_database()",
        )
        .expect("terminate other sessions");

    store.refresh().expect("retry on a new connection");
    assert_eq!(store.refresh_errors(), 0);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_poll_listener_picks_up_a_remote_write() {
    let (writer, _url, _c) = setup_pg_store().await;
    let reader = Arc::new(writer.clone());
    reader.refresh().unwrap();
    let _listener =
        PgExperimentStore::spawn_poll_listener(Arc::clone(&reader), Duration::from_millis(50));

    writer.upsert(running("polled")).unwrap();
    for _ in 0..200 {
        if reader.get("polled").unwrap().is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the poll listener did not refresh the reader");
}
