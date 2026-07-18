pub mod config;
pub mod error_reporter;
pub mod events;
pub mod experiments;
pub mod feature_flags;
pub mod hooks;
pub mod jobs;
pub mod listeners;
pub mod live_bus;
pub mod live_events;
pub mod models;
pub mod policies;
pub mod repositories;
pub mod routes;
pub mod schema;
pub mod slugify;
pub mod tasks;

use std::sync::{Arc, OnceLock};

use autumn_web::config::AutumnConfig;
use autumn_web::runtime_config::{ConfigStore, InMemoryConfigStore, RuntimeConfigService, pg};

static CONFIG_SVC: OnceLock<Arc<RuntimeConfigService>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeConfigStoreKind {
    InMemory,
    Postgres,
}

fn runtime_config_store_kind(config: &AutumnConfig) -> RuntimeConfigStoreKind {
    if config.database.effective_primary_url().is_some() {
        RuntimeConfigStoreKind::Postgres
    } else {
        RuntimeConfigStoreKind::InMemory
    }
}

fn runtime_config_store(config: &AutumnConfig) -> Arc<dyn ConfigStore> {
    match runtime_config_store_kind(config) {
        RuntimeConfigStoreKind::InMemory => Arc::new(InMemoryConfigStore::new()),
        RuntimeConfigStoreKind::Postgres => {
            let store = pg::PgConfigStore::from_database_config(&config.database)
                .expect("Postgres runtime config store requires a primary database URL");
            Arc::new(store)
        }
    }
}

fn build_config_service() -> Arc<RuntimeConfigService> {
    let app_config =
        AutumnConfig::load().expect("reddit-clone config must load before runtime config init");
    Arc::new(RuntimeConfigService::new(
        Arc::new(config::build_registry()),
        runtime_config_store(&app_config),
    ))
}

/// Initialise the config service.
///
/// The service is also initialized lazily by [`config_svc`], so command-line
/// modes that only inspect routes do not need to load runtime config first.
pub fn init_config() -> Arc<RuntimeConfigService> {
    Arc::clone(CONFIG_SVC.get_or_init(build_config_service))
}

/// Access the global config service from route handlers.
pub fn config_svc() -> &'static RuntimeConfigService {
    CONFIG_SVC.get_or_init(build_config_service).as_ref()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use autumn_web::config::{AutumnConfig, MockEnv};
    use diesel_migrations::{EmbeddedMigrations, embed_migrations};

    use super::{RuntimeConfigStoreKind, runtime_config_store_kind};
    use crate::live_bus::{LiveFeedBusConfig, LiveFeedBusKind};

    const _REDDIT_MIGRATIONS: EmbeddedMigrations = embed_migrations!();

    const MIGRATION_SQL: &str = include_str!("../migrations/20260419000000_create_reddit/up.sql");
    const MIGRATION_DOWN_SQL: &str =
        include_str!("../migrations/20260419000000_create_reddit/down.sql");

    #[test]
    fn migration_uses_bigserial_ids() {
        assert!(
            MIGRATION_SQL.contains("BIGSERIAL PRIMARY KEY"),
            "All IDs must be 64-bit to match the Int8/i64 application schema",
        );
    }

    #[test]
    fn migration_creates_all_tables() {
        for table in &[
            "users",
            "subreddits",
            "posts",
            "comments",
            "votes",
            "live_feed_events",
        ] {
            assert!(
                MIGRATION_SQL.contains(&format!("CREATE TABLE {table}")),
                "Migration must create the '{table}' table",
            );
        }
    }

    #[test]
    fn migration_down_drops_live_feed_events() {
        assert!(
            MIGRATION_DOWN_SQL.contains("DROP TABLE IF EXISTS live_feed_events"),
            "Rollback migration must drop the live_feed_events table added by up.sql",
        );
    }

    #[test]
    fn runtime_config_store_uses_postgres_when_database_url_configured() {
        let mut config = AutumnConfig::default();
        config.database.primary_url = Some("postgres://localhost/autumn".to_owned());

        assert_eq!(
            runtime_config_store_kind(&config),
            RuntimeConfigStoreKind::Postgres
        );
    }

    #[test]
    fn runtime_config_store_uses_memory_without_database_url() {
        let config = AutumnConfig::default();

        assert_eq!(
            runtime_config_store_kind(&config),
            RuntimeConfigStoreKind::InMemory
        );
    }

    #[test]
    fn main_registers_framework_migrations_for_runtime_config_tables() {
        let main_source = include_str!("main.rs");

        assert!(
            main_source.contains(".migrations(autumn_web::migrate::FRAMEWORK_MIGRATIONS)"),
            "reddit-clone must install framework migrations so the Postgres runtime config store has its tables"
        );
    }

    #[test]
    fn dev_profile_enables_csrf_for_forms() {
        let env = MockEnv::new()
            .with("AUTUMN_PROFILE", "dev")
            .with("AUTUMN_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"));

        let config =
            AutumnConfig::load_with_env(&env).expect("reddit-clone dev config should load");

        assert!(
            config.security.csrf.enabled,
            "reddit-clone extracts `CsrfToken`, so its dev profile must enable CSRF",
        );
    }

    #[test]
    fn redis_profile_uses_redis_jobs_backend() {
        let env = MockEnv::new()
            .with("AUTUMN_PROFILE", "redis")
            .with("AUTUMN_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"));

        let config =
            AutumnConfig::load_with_env(&env).expect("reddit-clone redis config should load");

        assert_eq!(config.jobs.backend, "redis");
        assert_eq!(config.jobs.workers, 2);
        assert_eq!(
            config.jobs.redis.url.as_deref(),
            Some("redis://127.0.0.1:6379/")
        );
    }

    #[test]
    fn redis_profile_declares_redis_live_feed_bus() {
        let env = MockEnv::new()
            .with("AUTUMN_PROFILE", "redis")
            .with("AUTUMN_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"));
        let config =
            LiveFeedBusConfig::load_with_env(&env).expect("redis live-feed bus config should load");

        assert_eq!(
            config.kind,
            LiveFeedBusKind::RedisPubSub,
            "redis profile should use an external live-feed bus instead of Postgres notify",
        );
        assert_eq!(config.redis_url.as_deref(), Some("redis://127.0.0.1:6379/"),);
    }

    #[test]
    fn default_live_feed_bus_uses_postgres_notify() {
        let env = MockEnv::new().with("AUTUMN_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"));
        let config = LiveFeedBusConfig::load_with_env(&env)
            .expect("default live-feed bus config should load");

        assert_eq!(config.kind, LiveFeedBusKind::PostgresNotify);
        assert_eq!(config.redis_url, None);
        assert_eq!(config.channel, "reddit_live_feed");
    }

    #[test]
    fn reddit_background_jobs_register_request_side_effects() {
        use crate::jobs::{PostPublicationJob, UserOnboardingJob};

        let jobs = crate::jobs::registered_jobs();
        let by_name = jobs
            .iter()
            .map(|job| {
                (
                    job.name.as_str(),
                    (job.max_attempts, job.initial_backoff_ms),
                )
            })
            .collect::<BTreeSet<_>>();

        assert_eq!(jobs.len(), 2);
        assert!(by_name.contains(&("user_onboarding", (5, 500))));
        assert!(by_name.contains(&("post_publication", (5, 500))));

        // NAME constants must match the registered job names so enqueue_on_conn callers
        // don't need to duplicate the string.
        assert_eq!(UserOnboardingJob::NAME, "user_onboarding");
        assert_eq!(PostPublicationJob::NAME, "post_publication");
    }

    #[test]
    fn default_profile_uses_postgres_jobs_backend() {
        let env = MockEnv::new().with("AUTUMN_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"));
        let config = AutumnConfig::load_with_env(&env).expect("default config should load");
        assert_eq!(
            config.jobs.backend, "postgres",
            "autumn.toml must set jobs.backend = \"postgres\" as the default"
        );
    }

    #[test]
    fn default_postgres_job_routes_enqueue_inside_user_transaction() {
        let auth_routes = include_str!("routes/auth.rs");
        let post_routes = include_str!("routes/posts.rs");

        for (name, source) in [("auth", auth_routes), ("posts", post_routes)] {
            assert!(
                source.contains("autumn_web::job::enqueue_on_conn"),
                "{name} routes must enqueue Postgres-backed jobs on the transaction connection"
            );
            assert!(
                !source.contains("autumn_web::job::enqueue_after_commit"),
                "{name} routes must not use post-commit job enqueue for the default Postgres backend"
            );
        }
    }
}
