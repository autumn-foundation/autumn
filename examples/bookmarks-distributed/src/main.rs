// Bookmarks Distributed - sibling example scaffolded from bookmarks so the
// future distributed retrofit has a clean, separate home:
//
//   Profiles        -> autumn.toml + autumn-dev.toml (dev auto-detected)
//   CRUD API        -> explicit /api/bookmarks handlers in repositories.rs
//   Scheduled tasks -> #[scheduled(every = "1h")] link health checker
//   Actuator        -> /actuator/health, /actuator/info, /actuator/env
//
// Run with:  cargo run -p bookmarks-distributed
// API test:  curl -X POST http://localhost:3000/api/bookmarks \
//              -H 'Content-Type: application/json' \
//              -d '{"url":"https://rust-lang.org","title":"Rust","tag":"lang"}'

mod config;
mod db;
mod models;
mod repositories;
mod routes;
mod schema;
mod state;
mod tasks;

use autumn_cache_redis::RedisCachePlugin;
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;
use std::sync::Arc;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

fn build_distributed_state() -> Arc<state::DistributedState> {
    let config = config::DistributedConfig::load()
        .expect("distributed example config should load from autumn.toml");
    let pools =
        db::create_dual_pools(&config).expect("distributed example pools should build from config");

    Arc::new(state::DistributedState::new(config, pools))
        .install_global()
        .expect("distributed state should only be installed once")
}

#[autumn_web::main]
async fn main() {
    let distributed_state = build_distributed_state();
    tracing::info!(
        primary_url_configured = distributed_state.config.database.primary_url.is_some(),
        replica_url_configured = distributed_state.config.database.replica_url.is_some(),
        configured_primary_pool_size = distributed_state.config.database.primary_pool_size,
        configured_replica_pool_size = distributed_state.config.database.replica_pool_size,
        primary_pool_size = distributed_state.pools.primary_pool_size(),
        replica_pool_size = distributed_state.pools.replica_pool_size(),
        "installed distributed bookmarks state"
    );

    // -- v0.2: .tasks() registers scheduled background tasks -----
    autumn_web::app()
        .plugin(RedisCachePlugin::new())
        .migrations(MIGRATIONS)
        .routes(routes![
            routes::bookmarks::list,
            routes::bookmarks::by_tag,
            routes::bookmarks::new_form,
            routes::bookmarks::create,
            // Self-clustering substrate (#1762): the local member view plus a
            // cluster-wide counter, shared between the two compose replicas
            // with no coordination service. See `src/routes/cluster.rs`.
            routes::cluster::status,
            repositories::bookmark_api_count,
            repositories::bookmark_api_list,
            repositories::bookmark_api_get,
            repositories::bookmark_api_create,
            repositories::bookmark_api_update,
            repositories::bookmark_api_delete,
        ])
        .tasks(tasks![tasks::check_links])
        .run()
        .await;
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use autumn_web::config::{AutumnConfig, MockEnv};

    /// Load the `docker` profile the compose stack runs under, with the
    /// per-instance values compose supplies as environment overrides.
    fn docker_config() -> AutumnConfig {
        let env = MockEnv::new()
            .with("AUTUMN_PROFILE", "docker")
            .with("AUTUMN_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"))
            .with("AUTUMN_CLUSTER__SECRET", "a-shared-cluster-secret-value-32")
            .with("AUTUMN_CLUSTER__NODE_ID", "web-2")
            .with("AUTUMN_CLUSTER__ADVERTISE_ADDR", "172.28.0.12:7946")
            .with("AUTUMN_CLUSTER__SEED_PEERS", "172.28.0.11:7946");
        AutumnConfig::load_with_env(&env).expect("docker profile config should load")
    }

    /// The compose stack's two web replicas are meant to form a cluster, so
    /// the profile they boot under has to actually turn it on.
    #[test]
    fn docker_profile_enables_the_cluster_substrate() {
        let config = docker_config();

        assert!(
            config.cluster.enabled,
            "the docker profile must enable [cluster] — that is what makes \
             bookmarks-1 and bookmarks-2 a two-node cluster"
        );
        assert_eq!(config.cluster.cluster_name, "bookmarks-distributed");
        assert_eq!(config.cluster.bind_addr, "0.0.0.0:7946");
        assert_eq!(
            config.cluster.seed_peers,
            vec!["172.28.0.11:7946".to_owned()],
            "seed_peers arrives as a comma-separated environment override"
        );
    }

    /// `[cluster]` is validated at boot, and a bad section is a startup error
    /// rather than a warning — so a committed profile that would fail
    /// validation takes the whole compose stack down on first run.
    #[test]
    fn docker_profile_cluster_section_passes_boot_validation() {
        docker_config()
            .cluster
            .validate()
            .expect("the committed [cluster] section must satisfy boot validation");
    }

    /// A wildcard bind is not an address a peer can dial, so every replica
    /// must advertise a concrete one. `docker-compose.yml` gives each replica a
    /// fixed IP on the `bookmarks` network for exactly this reason: the config
    /// parses socket addresses and does not resolve hostnames, so the service
    /// names the rest of the stack uses would not work here.
    #[test]
    fn compose_gives_each_replica_a_distinct_dialable_identity() {
        let compose = include_str!("../docker-compose.yml");

        for (node_id, ip) in [("web-1", "172.28.0.11"), ("web-2", "172.28.0.12")] {
            assert!(
                compose.contains(&format!("AUTUMN_CLUSTER__NODE_ID: {node_id}")),
                "compose must give each replica a stable node id; {node_id} is missing"
            );
            assert!(
                compose.contains(&format!("AUTUMN_CLUSTER__ADVERTISE_ADDR: {ip}:7946")),
                "compose must advertise a dialable address for {node_id}"
            );
            assert!(
                compose.contains(&format!("ipv4_address: {ip}")),
                "{node_id} needs a fixed address — [cluster] does not resolve hostnames"
            );
        }

        assert!(
            compose.contains("AUTUMN_CLUSTER__SEED_PEERS: \"172.28.0.11:7946\""),
            "seeding one direction is enough, and it has to point at the other node"
        );
    }

    const MIGRATION_SQL: &str =
        include_str!("../migrations/00000000000000_create_bookmarks/up.sql");

    #[test]
    fn migration_uses_bigserial_ids() {
        assert!(
            MIGRATION_SQL.contains("id BIGSERIAL PRIMARY KEY"),
            "bookmark IDs must be 64-bit to match the Int8/i64 application schema",
        );
    }

    #[test]
    fn upgrade_migration_widens_existing_ids() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("migrations/00000000000001_widen_bookmark_ids_to_bigint/up.sql");
        let sql = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("missing upgrade migration at {}: {err}", path.display()));

        assert!(
            sql.contains("ALTER TABLE bookmarks ALTER COLUMN id TYPE BIGINT"),
            "bookmark upgrade migration must widen existing IDs to BIGINT",
        );
        assert!(
            sql.contains("ALTER SEQUENCE bookmarks_id_seq AS BIGINT"),
            "bookmark upgrade migration must widen the backing sequence to BIGINT",
        );
    }
}
