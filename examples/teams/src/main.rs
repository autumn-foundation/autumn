use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;
use teams::routes;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .migrations(MIGRATIONS)
        .routes(routes![
            teams::index,
            routes::auth::signup_form,
            routes::auth::signup,
            routes::auth::login_form,
            routes::auth::login,
            routes::auth::logout,
            routes::organizations::create_organization,
            routes::organizations::switch_organization,
            routes::invitations::create_invitation,
            routes::invitations::show_invitation,
            routes::invitations::accept_invitation,
            routes::invitations::revoke_invitation,
            routes::invitations::resend_invitation,
            routes::members::list_members,
            routes::members::change_role,
            routes::members::remove_member,
        ])
        .run()
        .await;
}
