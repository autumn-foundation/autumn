//! Boot-time shard-map guard (issue #1277).
//!
//! Verifies the `_autumn_shard_map` control table is written on first boot
//! and that a topology change under auto-split causes startup to refuse with a
//! clear error message, while leaving the stored map untouched.
//!
//! The control DB requires Docker; the shard pools are never connected to
//! (deadpool builds them lazily), so only one container is needed.
//!
//! Run with:
//!
//!     cargo test --test shard_map_guard --features db,test-support -- --include-ignored

#![cfg(feature = "db")]

#[cfg(feature = "test-support")]
use autumn_web::config::{DatabaseConfig, ShardConfig};
#[cfg(feature = "test-support")]
use autumn_web::test::TestDb;
#[cfg(feature = "test-support")]
use diesel_async::RunQueryDsl;

#[cfg(feature = "test-support")]
fn two_shard_config() -> DatabaseConfig {
    DatabaseConfig {
        connect_timeout_secs: 1,
        shards: ["shard0", "shard1"]
            .iter()
            .map(|name| ShardConfig {
                name: (*name).to_owned(),
                primary_url: format!("postgres://localhost/{name}"),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

#[cfg(feature = "test-support")]
fn three_shard_config() -> DatabaseConfig {
    DatabaseConfig {
        connect_timeout_secs: 1,
        shards: ["shard0", "shard1", "shard2"]
            .iter()
            .map(|name| ShardConfig {
                name: (*name).to_owned(),
                primary_url: format!("postgres://localhost/{name}"),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

/// Retrieve rows from `_autumn_shard_map`, sorted by `shard_name`.
#[cfg(feature = "test-support")]
#[derive(diesel::QueryableByName)]
struct ShardMapRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    shard_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    slots: String,
}

#[cfg(feature = "test-support")]
async fn read_stored_map(db: &TestDb) -> Vec<(String, String)> {
    let mut conn = db.pool().get().await.expect("control connection");
    diesel::sql_query("SELECT shard_name, slots FROM _autumn_shard_map ORDER BY shard_name")
        .load::<ShardMapRow>(&mut conn)
        .await
        .expect("read _autumn_shard_map")
        .into_iter()
        .map(|r| (r.shard_name, r.slots))
        .collect()
}

#[cfg(feature = "test-support")]
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn shard_map_guard_persists_on_first_boot_and_reboot_is_ok() {
    let db = TestDb::shared().await;
    db.execute_sql(include_str!(
        "../../migrations/20260627000000_create_shard_map/up.sql"
    ))
    .await;

    let config = two_shard_config();
    let computed = config
        .resolved_shard_assignments()
        .expect("two-shard auto-split should resolve");

    // First boot: no rows → guard should succeed and persist.
    autumn_web::app::run_shard_map_guard(&db.pool(), &computed, true)
        .await
        .expect("first boot should succeed");

    let stored = read_stored_map(db).await;
    assert_eq!(stored.len(), 2, "two rows should be persisted");
    assert_eq!(stored[0].0, "shard0");
    assert_eq!(stored[0].1, "0-8191");
    assert_eq!(stored[1].0, "shard1");
    assert_eq!(stored[1].1, "8192-16383");

    // Second boot with the same config: should still succeed.
    autumn_web::app::run_shard_map_guard(&db.pool(), &computed, true)
        .await
        .expect("matching reboot should succeed");

    // Stored map must be unchanged.
    assert_eq!(read_stored_map(db).await, stored);
}

#[cfg(feature = "test-support")]
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn shard_map_guard_refuses_when_topology_changes() {
    let db = TestDb::shared().await;
    db.execute_sql(include_str!(
        "../../migrations/20260627000000_create_shard_map/up.sql"
    ))
    .await;

    // Seed a 2-shard map as if from a prior boot.
    {
        let mut conn = db.pool().get().await.expect("control connection");
        for (name, slots) in [("shard0", "0-8191"), ("shard1", "8192-16383")] {
            diesel::sql_query(
                "INSERT INTO _autumn_shard_map (shard_name, slots) VALUES ($1, $2) \
                 ON CONFLICT (shard_name) DO UPDATE SET slots = EXCLUDED.slots",
            )
            .bind::<diesel::sql_types::Text, _>(name)
            .bind::<diesel::sql_types::Text, _>(slots)
            .execute(&mut *conn)
            .await
            .expect("seed map row");
        }
    }

    let config = three_shard_config();
    let computed = config
        .resolved_shard_assignments()
        .expect("three-shard auto-split should resolve");

    let err = autumn_web::app::run_shard_map_guard(&db.pool(), &computed, true)
        .await
        .expect_err("3-shard auto-split vs 2-shard stored map must fail");

    assert!(
        err.contains("shard slot map mismatch"),
        "error must describe the mismatch; got: {err}"
    );
    assert!(
        err.contains("3 shards"),
        "must mention computed count; got: {err}"
    );
    assert!(
        err.contains("2 shards"),
        "must mention stored count; got: {err}"
    );

    // Stored map must be untouched — no rows added or modified.
    let stored = read_stored_map(db).await;
    assert_eq!(
        stored.len(),
        2,
        "stored map must be unchanged after mismatch"
    );
    assert_eq!(stored[0].1, "0-8191");
    assert_eq!(stored[1].1, "8192-16383");
}

#[cfg(feature = "test-support")]
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn shard_map_guard_inert_in_explicit_slot_mode() {
    let db = TestDb::shared().await;
    db.execute_sql(include_str!(
        "../../migrations/20260627000000_create_shard_map/up.sql"
    ))
    .await;

    // Seed a 2-shard map so there is something to compare against.
    {
        let mut conn = db.pool().get().await.expect("control connection");
        for (name, slots) in [("shard0", "0-8191"), ("shard1", "8192-16383")] {
            diesel::sql_query(
                "INSERT INTO _autumn_shard_map (shard_name, slots) VALUES ($1, $2) \
                 ON CONFLICT (shard_name) DO UPDATE SET slots = EXCLUDED.slots",
            )
            .bind::<diesel::sql_types::Text, _>(name)
            .bind::<diesel::sql_types::Text, _>(slots)
            .execute(&mut *conn)
            .await
            .expect("seed map row");
        }
    }

    // A completely different computed map, but auto_split = false (explicit mode).
    let different_map = three_shard_config()
        .resolved_shard_assignments()
        .expect("resolve");
    autumn_web::app::run_shard_map_guard(&db.pool(), &different_map, false)
        .await
        .expect("explicit mode must always succeed");
}
