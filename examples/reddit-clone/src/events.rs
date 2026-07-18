//! Typed domain events for reddit-clone.
//!
//! Events describe *something that happened* in the app. Handlers publish them;
//! decoupled listeners (see [`crate::listeners`]) react. Adding a new reaction
//! is a new listener and zero edits to the handler that publishes the event.

use autumn_web::event;

/// Emitted right after a new account is created in the signup handler.
#[event]
pub struct UserSignedUp {
    pub user_id: i64,
    pub username: String,
}
