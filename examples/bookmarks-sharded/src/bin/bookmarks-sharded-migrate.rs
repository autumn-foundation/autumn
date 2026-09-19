//! One-shot migration job: applies the embedded migrations to the control
//! database and then to every `[[database.shards]]` entry, in declaration
//! order, failing fast. Run before the web replicas start (the compose
//! stack wires this as `bookmarks-migrate`).

use autumn_web::config::AutumnConfig;
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations, run_pending};

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

fn main() {
    let config = AutumnConfig::load().unwrap_or_else(|error| {
        eprintln!("failed to load autumn.toml: {error}");
        std::process::exit(1);
    });

    let mut targets: Vec<(String, String)> = Vec::new();
    if let Some(control) = config.database.effective_primary_url() {
        targets.push(("control".to_owned(), control.to_owned()));
    }
    for shard in &config.database.shards {
        targets.push((format!("shard:{}", shard.name), shard.primary_url.clone()));
    }

    for (label, url) in targets {
        eprintln!("migrating {label}...");
        // `MIGRATIONS` is a const, so each use materializes a fresh copy —
        // one embedded set applies to every target.
        let result = run_pending(&url, MIGRATIONS).unwrap_or_else(|error| {
            eprintln!("migration failed on {label}: {error}");
            std::process::exit(1);
        });
        if result.applied.is_empty() {
            eprintln!("  {label}: up to date");
        } else {
            for name in &result.applied {
                eprintln!("  {label}: applied {name}");
            }
        }
    }
    eprintln!("all targets migrated");
}
