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
        .migrations(MIGRATIONS)
        .routes(wiki::all_routes())
        .static_routes(static_routes![wiki::routes::docs::show])
        .run()
        .await;
}
