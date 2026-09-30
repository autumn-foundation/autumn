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
        // `commit_hooks = true` on `PageRepository` (src/repositories.rs)
        // needs the framework's repository-commit-hook-queue table, but that
        // is auto-registered at startup whenever any repository in the
        // binary declares `commit_hooks = true`
        // (`repository_commit_hooks::has_repository_commit_hook_descriptors`)
        // — no explicit `.migrations(FRAMEWORK_MIGRATIONS)` needed, and
        // deliberately not added: `FRAMEWORK_MIGRATIONS` also carries
        // `00000000000000_create_api_tokens`, which collides on Diesel's
        // version-keyed `__diesel_schema_migrations` with this crate's own
        // `00000000000000_create_wiki` (a `00000000000000` collision already
        // grandfathered — not renamed — across the framework, the starters,
        // and eight examples per `scripts/check-migration-versions.sh`;
        // whichever migration ran first would silently "win" and the other
        // would never create its tables).
        .migrations(MIGRATIONS)
        .plugin(wiki::search_plugin())
        .routes(wiki::all_routes())
        .static_routes(static_routes![wiki::routes::docs::show])
        .run()
        .await;
}
