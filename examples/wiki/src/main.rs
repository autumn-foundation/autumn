//! Example Wiki application demonstrating the Autumn web framework.
//!
//! This example shows how to build a typical server-side rendered application
//! with forms, database access, and HTML templates.
//!
//! The app itself lives in `src/lib.rs` (`wiki::all_routes()`); this binary
//! just wires migrations and starts the server.

use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        // Required by `commit_hooks = true` (src/repositories.rs) and by
        // `[jobs] backend = "postgres"` (autumn.toml): the durable
        // repository-commit-hook queue and the Postgres job-queue tables
        // both live in the framework's own migration set, not wiki's.
        // Matches examples/reddit-clone's main.rs.
        .migrations(autumn_web::migrate::FRAMEWORK_MIGRATIONS)
        .migrations(MIGRATIONS)
        .plugin(wiki::search_plugin())
        .routes(wiki::all_routes())
        .static_routes(static_routes![wiki::routes::docs::show])
        .run()
        .await;
}
