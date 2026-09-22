//! Example Wiki application demonstrating the Autumn web framework.
//!
//! This example shows how to build a typical server-side rendered application
//! with forms, database access, and HTML templates.
//!
//! Split into a library (this crate) plus a thin `src/main.rs`, mirroring
//! `examples/cms`'s split: a Ledger profiling harness needs to mount the same
//! route table the binary serves, from a separate `tests/` crate, which only a
//! `[lib]` target makes possible.

mod hooks;
mod models;
pub mod repositories;
pub mod routes;
mod schema;

/// Every route the application serves.
///
/// Defined here rather than inline in `main` so the binary and any test that
/// mounts the app (e.g. a profiling harness under `tests/`) use the same
/// table.
#[must_use]
pub fn all_routes() -> Vec<autumn_web::Route> {
    autumn_web::routes![
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
    ]
}
