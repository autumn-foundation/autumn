mod metrics;
mod models;
mod repositories;
mod routes;
mod schema;
mod tasks;
mod wizards;

use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::openapi::OpenApiConfig;
use autumn_web::prelude::*;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[get("/")]
async fn home() -> Redirect {
    Redirect::to("/bookmarks")
}

#[autumn_web::main]
async fn main() {
    // Attach `# HELP` text and bucket bounds to this example's own
    // instruments before anything records into them. Recording itself needs
    // no registration at all — see `src/metrics.rs`.
    metrics::describe();

    autumn_web::app()
        .migrations(MIGRATIONS)
        .routes(routes![
            home,
            routes::bookmarks::index,
            routes::bookmarks::export_csv,
            // Grouped-aggregate roll-up page (#1364): per-tag counts and a
            // day-bucketed activity series, both via the aggregate API.
            routes::bookmarks::stats,
            routes::bookmarks::search,
            routes::bookmarks::tags_autocomplete,
            routes::bookmarks::show,
            routes::bookmarks::by_tag,
            routes::bookmarks::new_form,
            routes::bookmarks::create,
            routes::bookmarks::edit_form,
            routes::bookmarks::update,
            repositories::bookmark::bookmark_api_list,
            repositories::bookmark::bookmark_api_get,
            repositories::bookmark::bookmark_api_create,
            repositories::bookmark::bookmark_api_update,
            repositories::bookmark::bookmark_api_delete,
            wizards::add_bookmark::show_url,
            wizards::add_bookmark::submit_url,
            wizards::add_bookmark::show_details,
            wizards::add_bookmark::submit_details,
            wizards::add_bookmark::show_confirm,
            wizards::add_bookmark::commit,
            wizards::add_bookmark::cancel,
        ])
        .tasks(tasks![tasks::check_links])
        // Serves the derived spec at `/openapi.json` and a Swagger UI at
        // `/swagger-ui`, covering the `#[repository]`-generated JSON handlers
        // registered above. See `docs/guide/openapi.md`.
        .openapi(OpenApiConfig::new("Bookmarks API", "1.0.0"))
        .run()
        .await;
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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
