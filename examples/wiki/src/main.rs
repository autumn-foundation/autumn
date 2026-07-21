//! Example Wiki application demonstrating the Autumn web framework.
//!
//! This example shows how to build a typical server-side rendered application
//! with forms, database access, and HTML templates.

mod hooks;
mod models;
mod repositories;
mod routes;
mod schema;
mod slugify;

use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .migrations(MIGRATIONS)
        .routes(routes![
            routes::pages::list,
            routes::pages::show,
            routes::pages::new_form,
            routes::pages::create,
            routes::pages::edit_form,
            routes::pages::update,
            routes::pages::transition_status,
            routes::pages::history,
            routes::pages::search,
            routes::collections::list,
            routes::collections::new_form,
            routes::collections::create,
            routes::collections::show,
            routes::collections::edit_form,
            routes::collections::update,
            repositories::page_api_list,
            repositories::page_api_get,
            repositories::page_api_create,
            repositories::page_api_update,
            repositories::page_api_delete,
            routes::docs::show,
            routes::docs::index,
        ])
        .static_routes(static_routes![routes::docs::show])
        .run()
        .await;
}
