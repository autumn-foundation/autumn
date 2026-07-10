use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::middleware::from_fn;
use {{crate_name}}::routes;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .migrations(MIGRATIONS)
        .routes(routes![
            {{crate_name}}::index,
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
        .layer(from_fn({{crate_name}}::remember::remember_me_middleware))
        .on_startup(|state| async move {
            if let Some(pool) = state.pool() {
                // Hand the middleware the resolved `[auth.remember]` config too,
                // so cookie_name/duration overrides are honoured (issue #1397.2).
                {{crate_name}}::remember::init_remember_pool(
                    pool.clone(),
                    state.config().auth.remember.clone(),
                );
            }
            Ok(())
        })
        .run()
        .await;
}
