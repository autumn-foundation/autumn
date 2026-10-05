//! A stockroom app that shows each Autumn escape hatch with a real reason.
//!
//! Read `PLAN.md` first. Each hatch has a number (H1..H13).

pub mod api;
pub mod hatches;
pub mod models;
pub mod pages;
pub mod reports;
pub mod repositories;
pub mod schema;
pub mod supplier;

use autumn_web::app::AppBuilder;
use autumn_web::auth::RequireApiToken;
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;

/// The stockroom migration.
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

/// Browser routes and the report.
#[must_use]
pub fn routes() -> Vec<autumn_web::Route> {
    routes![pages::index, pages::product, reports::stock_value]
}

/// Scanner routes. `app()` mounts them under `/api`.
#[must_use]
pub fn api_routes() -> Vec<autumn_web::Route> {
    routes![api::reserve, api::checkout, api::restock, api::order]
}

/// H5: the bearer-token guard for scanner devices.
#[must_use]
pub fn scanner_guard(_token: Option<&str>) -> RequireApiToken {
    todo!("H5")
}

/// Build the app.
#[must_use]
pub fn app() -> AppBuilder {
    autumn_web::app().migrations(MIGRATIONS).routes(routes())
}
