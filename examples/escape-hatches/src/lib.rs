//! A stockroom app that shows each Autumn escape hatch with a real reason.
//!
//! Read `PLAN.md` first. Each hatch has a number (H1..H13). The convention
//! comes first in each file. The hatch comes only where the convention
//! cannot do the job, and a comment says why.

pub mod api;
pub mod hatches;
pub mod models;
pub mod pages;
pub mod reports;
pub mod repositories;
pub mod schema;
pub mod supplier;

use std::sync::Arc;

use autumn_web::app::AppBuilder;
use autumn_web::auth::{InMemoryApiTokenStore, RequireApiToken};
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;

use crate::hatches::error_pages::StockroomErrorPages;
use crate::hatches::password_file::PasswordFilePool;
use crate::hatches::retry_after::RetryAfterOn503;
use crate::hatches::supplier_plugin::SupplierPlugin;
use crate::hatches::{cache_control, exports};

/// The stockroom migration.
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

/// The env var that holds the scanner bearer token.
pub const SCANNER_TOKEN_ENV: &str = "STOCKROOM_SCANNER_TOKEN";

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

/// H5: the bearer-token guard for scanner devices, as a `scoped` layer.
///
/// Browser pages need no login in this demo. Scanners are machines: they
/// send `Authorization: Bearer <token>` and keep no session. `scoped` puts
/// the framework's `RequireApiToken` layer on the `/api` group only.
///
/// With no token, the guard refuses every call. It never opens the API.
#[must_use]
pub fn scanner_guard(token: Option<&str>) -> RequireApiToken {
    let store = match token {
        Some(token) if !token.is_empty() => {
            InMemoryApiTokenStore::default().with_token(token, "scanner")
        }
        _ => InMemoryApiTokenStore::default(),
    };
    RequireApiToken::new(Arc::new(store))
}

/// Build the app. Each line after `routes` is one escape hatch.
#[must_use]
pub fn app() -> AppBuilder {
    let scanner_token = std::env::var(SCANNER_TOKEN_ENV).ok();
    if scanner_token.is_none() {
        tracing::warn!("{SCANNER_TOKEN_ENV} is not set; the /api routes refuse every call");
    }
    autumn_web::app()
        .migrations(MIGRATIONS)
        .routes(routes())
        // H5: bearer tokens on the scanner API only.
        .scoped(
            "/api",
            scanner_guard(scanner_token.as_deref()),
            api_routes(),
        )
        // H8: runtime export files, from a nested raw router.
        .nest(exports::PREFIX, exports::router(&exports::dir_from_env()))
        .declare_plugin_routes(exports::routes())
        // H9: the supplier's plain Axum router, as a plugin.
        .plugin(SupplierPlugin::sample())
        // H7: no shared cache keeps a page.
        .layer(cache_control::no_store())
        // H10: error pages in the stockroom frame.
        .error_pages(StockroomErrorPages)
        // H11: every 503 tells the client when to retry.
        .exception_filter(RetryAfterOn503)
        // H12: the database password comes from a rotated file.
        .with_pool_provider(PasswordFilePool::from_env())
}
