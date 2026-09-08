//! Autumn CMS — a WordPress-core-parity content management system.
//!
//! Run it:
//!
//! ```bash
//! docker compose up -d   # Postgres
//! autumn migrate
//! autumn dev             # http://localhost:3000
//! ```
//!
//! The first account to register owns the site (Administrator). Everything
//! after that is done from `/admin`.

use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[autumn_web::main]
async fn main() {
    // Registrations happen before the router is built, which is what makes a
    // custom post type appear in the admin menu, the router and the REST API
    // at once — the whole point of a registry.
    cms::bootstrap();

    autumn_web::app()
        .migrations(MIGRATIONS)
        // The settings read is memoized per process (see
        // `settings::cached_settings`); this is the backend it lives in. Swap
        // in the Redis backend to share it across replicas.
        .with_cache_backend(autumn_web::cache::MokaCache::new(1_000, None))
        .routes(cms::all_routes())
        .tasks(tasks![cms::tasks::publish_scheduled])
        .one_off_tasks(one_off_tasks![cms::seed::seed_demo_content])
        .run()
        .await;
}
