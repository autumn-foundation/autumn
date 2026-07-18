//! Event listeners for reddit-clone.
//!
//! Each listener reacts to a typed event independently of the code that emits
//! it. This file is the *only* thing that changes when a new reaction is added —
//! the signup handler that publishes `UserSignedUp` is never touched.

use autumn_web::prelude::*;
use autumn_web::{listener, listeners};

use crate::events::UserSignedUp;

/// Durable reaction: rides the `#[job]` queue, so it survives a process restart
/// and inherits the queue's retry + DLQ semantics. In a real app this might
/// seed a default subreddit subscription or send a welcome email.
#[listener(UserSignedUp, durable, max_attempts = 5, backoff_ms = 500)]
async fn welcome_new_user(_state: AppState, event: UserSignedUp) -> AutumnResult<()> {
    tracing::info!(
        user_id = event.user_id,
        username = %event.username,
        "welcoming newly signed-up user (durable listener)"
    );
    Ok(())
}

/// Synchronous reaction: runs in-request, before the response. Use for
/// invariants the caller depends on. Records a lightweight signup metric.
#[listener(UserSignedUp)]
async fn record_signup_metric(_state: AppState, event: UserSignedUp) -> AutumnResult<()> {
    tracing::debug!(
        user_id = event.user_id,
        "recording signup metric (sync listener)"
    );
    Ok(())
}

/// Collected for `AppBuilder::listeners`, mirroring `jobs::registered_jobs`.
#[must_use]
pub fn registered_listeners() -> Vec<autumn_web::events::ListenerInfo> {
    listeners![welcome_new_user, record_signup_metric]
}

#[cfg(test)]
mod tests {
    use autumn_web::events::DispatchMode;

    #[test]
    fn registers_both_signup_listeners() {
        let listeners = super::registered_listeners();
        assert_eq!(listeners.len(), 2);

        let durable = listeners
            .iter()
            .filter(|l| l.mode == DispatchMode::Durable)
            .count();
        let sync = listeners
            .iter()
            .filter(|l| l.mode == DispatchMode::Sync)
            .count();
        assert_eq!(durable, 1, "welcome_new_user is durable");
        assert_eq!(sync, 1, "record_signup_metric is sync");

        // Every listener subscribes to UserSignedUp. The default event name is
        // module-qualified, so match on the suffix rather than the bare type.
        assert!(
            listeners
                .iter()
                .all(|l| l.event_name.ends_with("::UserSignedUp"))
        );
    }
}
