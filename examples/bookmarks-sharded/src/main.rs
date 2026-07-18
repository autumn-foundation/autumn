// Bookmarks Sharded — framework-native horizontal sharding.
//
// The point of this example is what's *missing*: there are no custom
// pools, no routing code, and no shard bookkeeping in the application.
// Sharding is declared in autumn.toml ([[database.shards]] + slots) and
// the handlers use `ShardedDb` / `Shards` from the prelude.
//
//   Run locally:  docker compose -f examples/bookmarks-sharded/docker-compose.yml up -d --build
//   Create:       curl -X POST http://localhost:3000/api/bookmarks \
//                   -H 'Content-Type: application/json' -H 'X-Tenant-Id: acme' \
//                   -d '{"url":"https://rust-lang.org","title":"Rust","tag":"lang"}'
//   List:         curl -H 'X-Tenant-Id: acme' http://localhost:3000/api/bookmarks
//   Fan-out:      curl http://localhost:3000/api/stats

mod models;
mod routes;
mod schema;

use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .migrations(MIGRATIONS)
        .routes(routes![
            routes::bookmarks::list,
            routes::bookmarks::create,
            routes::admin::stats,
        ])
        .run()
        .await;
}
