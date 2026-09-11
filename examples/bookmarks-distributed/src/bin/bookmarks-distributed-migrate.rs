#[allow(dead_code)]
#[path = "../config.rs"]
mod config;

use autumn_web::migrate::{EmbeddedMigrations, embed_migrations, run_pending};

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

fn primary_database_url(
    config: &config::DistributedConfig,
) -> Result<&str, config::MissingDistributedDatabaseUrls> {
    config.database.urls().map(|(primary_url, _)| primary_url)
}

fn main() {
    let config = config::DistributedConfig::load()
        .expect("distributed example config should load for migration");
    let primary_url =
        primary_database_url(&config).expect("distributed example requires a primary database URL");

    eprintln!("running bookmarks-distributed migrations against primary database");

    let result = run_pending(primary_url, MIGRATIONS).unwrap_or_else(|error| {
        eprintln!("migration failed: {error}");
        std::process::exit(1);
    });

    if result.applied.is_empty() {
        eprintln!("no pending migrations");
    } else {
        for name in result.applied {
            eprintln!("applied migration: {name}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::primary_database_url;
    use crate::config::{DistributedConfig, DistributedDatabaseConfig};

    #[test]
    fn migrate_binary_uses_primary_url() {
        let config = DistributedConfig::from_urls(
            "postgres://autumn:autumn@primary:5432/bookmarks_distributed",
            "postgres://autumn:autumn@replica:5432/bookmarks_distributed",
        )
        .with_pool_sizes(8, 4);

        assert_eq!(
            primary_database_url(&config).expect("primary URL should be present"),
            "postgres://autumn:autumn@primary:5432/bookmarks_distributed"
        );
    }

    #[test]
    fn migrate_binary_rejects_missing_primary_url() {
        let config = DistributedConfig {
            database: DistributedDatabaseConfig {
                primary_url: None,
                replica_url: Some(
                    "postgres://autumn:autumn@replica:5432/bookmarks_distributed".to_owned(),
                ),
                ..DistributedDatabaseConfig::default()
            },
        };

        assert_eq!(
            primary_database_url(&config)
                .expect_err("missing primary URL should be rejected")
                .to_string(),
            "primary database URL is required"
        );
    }
}
