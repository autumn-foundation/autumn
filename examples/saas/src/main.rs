use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::middleware::from_fn;
use saas::routes;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .migrations(MIGRATIONS)
        .routes(routes![
            saas::index,
            routes::auth::signup_form,
            routes::auth::signup,
            routes::auth::login_form,
            routes::auth::login,
            routes::auth::logout,
            routes::dashboard::dashboard,
            routes::dashboard::create_project,
        ])
        // Persistent "remember-me" (issue #1397): rotate a valid remember cookie
        // into a fresh session BEFORE the tenancy gate runs, so a returning
        // visitor whose session cookie has expired still reaches their dashboard.
        // The startup hook hands the middleware the shared pool — it runs as a
        // plain Tower layer with no `AppState` access.
        .layer(from_fn(saas::remember::remember_me_middleware))
        .on_startup(|state| async move {
            if let Some(pool) = state.pool() {
                saas::remember::init_remember_pool(pool.clone());
            }
            Ok(())
        })
        .run()
        .await;
}
