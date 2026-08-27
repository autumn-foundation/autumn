//! Web Push: deliver a notification to a subscribed browser **even when the
//! app's tab is closed** (issue #1392).
//!
//! Autumn already generates an installable PWA (`autumn generate pwa`) and
//! stores an in-app notification feed
//! ([`notifications`](crate::notifications)). This module closes the loop
//! between them: the browser subscribes, the app calls
//! [`WebPush::send`], and the service worker raises a notification on the
//! user's device. The developer writes **zero** lines of crypto — VAPID
//! signing (RFC 8292) and payload encryption (RFC 8291) both happen here.
//!
//! # Quick start
//!
//! 1. Mint a key pair once, offline, and record the private half:
//!
//!    ```rust,ignore
//!    let key = autumn_web::push::VapidKey::generate();
//!    println!("private_key = \"{}\"", key.private_key_base64url());
//!    ```
//!
//! 2. Configure it (see [`PushConfig`]):
//!
//!    ```toml
//!    [push]
//!    private_key = "…"                    # from step 1; keep it secret
//!    subject     = "mailto:ops@example.com"
//!    ```
//!
//! 3. Mount the built-in routes — `autumn generate pwa` does this for you:
//!
//!    ```rust,ignore
//!    autumn_web::app()
//!        .merge(autumn_web::push::router())
//!        .run()
//!        .await;
//!    ```
//!
//! 4. Send:
//!
//!    ```rust,ignore
//!    use autumn_web::prelude::*;
//!
//!    #[post("/builds/{id}/fail")]
//!    async fn build_failed(push: WebPush, id: Path<i64>) -> AutumnResult<&'static str> {
//!        push.send(
//!            owner_id,
//!            &PushMessage::new("Build failed", "main is red")
//!                .url(format!("/builds/{}", *id)),
//!        )
//!        .await?;
//!        Ok("ok")
//!    }
//!    ```
//!
//! # Composing with the in-app notification feed (#1148)
//!
//! Push is a *delivery leg*, not a replacement for the feed: the feed is the
//! durable record the user can come back to, and the push is the nudge that
//! brings them back. Write the notification first, then push best-effort —
//! mirroring how [`Notifications::notify_with_push`] treats its `channels`
//! broadcast:
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::push::PushMessage;
//!
//! #[post("/posts/{id}/comments")]
//! async fn comment(
//!     notifications: Notifications,
//!     push: WebPush,
//!     id: Path<i64>,
//! ) -> AutumnResult<&'static str> {
//!     // 1. The durable record. A failure here IS a failure of the request.
//!     notifications
//!         .notify(author_id, "comment.created", serde_json::json!({ "post": *id }))
//!         .await?;
//!
//!     // 2. The nudge. Best-effort: a push service outage, a revoked
//!     //    permission, or an app with no `[push]` key configured must never
//!     //    fail the comment that was already written. `PushPrincipal` accepts
//!     //    the same `i64` recipient the feed uses, so there is no conversion.
//!     if let Err(e) = push
//!         .send(
//!             author_id,
//!             &PushMessage::new("New comment", "Someone replied to your post")
//!                 .url(format!("/posts/{}", *id)),
//!         )
//!         .await
//!     {
//!         tracing::warn!(error = %e, "web push failed; the in-app notification still stands");
//!     }
//!
//!     Ok("ok")
//! }
//! ```
//!
//! The ordering is the point: the feed write is awaited and propagated, the
//! push is awaited and *logged*. A user whose browser has never subscribed
//! gets an empty [`PushDeliveryReport`] rather than an error, so the branch
//! above only fires on a genuine problem.
//!
//! # What is stored, and where
//!
//! A browser subscription is an endpoint URL plus two keys, bound to a
//! principal — see [`PushSubscriptionStore`]. The default backend is the
//! `push_subscriptions` table scaffolded by `autumn generate pwa`, falling
//! back to an in-memory store when no database is configured.
//!
//! # Failure posture
//!
//! - **Never a silent no-op.** Sending with no key configured is
//!   [`PushError::NotConfigured`], raised before anything is dispatched, and a
//!   key that is present but unusable fails the boot outright.
//! - **A dead device never blocks a live one.** Per-device outcomes are
//!   reported through [`PushDeliveryReport`], not raised.
//! - **Stale subscriptions are pruned, transient failures are not.** A
//!   `404`/`410` removes the subscription; a `5xx` or a rate limit leaves it
//!   in place to retry, because pruning on those would silently unsubscribe
//!   every user during an incident.

pub mod config;
pub(crate) mod encryption;
pub mod router;
pub mod service;
pub mod store;
pub mod transport;
pub mod vapid;

pub use config::PushConfig;
pub use router::router;
pub use service::{PushDeliveryReport, PushMessage, WebPush};
#[cfg(feature = "db")]
pub use store::DbPushSubscriptionStore;
pub use store::{
    BrowserSubscription, MAX_SUBSCRIPTIONS_PER_PRINCIPAL, MemoryPushSubscriptionStore,
    PUSH_SUBSCRIPTIONS_TABLE, PushPrincipal, PushSubscriptionStore, StoredSubscription,
    SubscriptionKeys,
};
pub use transport::{PushRequest, PushTransport, RecordingPushTransport};
pub use vapid::VapidKey;

/// Errors produced by the Web Push subsystem.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PushError {
    /// The configured VAPID key could not be loaded.
    #[error("invalid VAPID key: {0}")]
    InvalidVapidKey(String),
    /// A subscription endpoint URL is unusable.
    #[error("invalid push endpoint: {0}")]
    InvalidEndpoint(String),
    /// A browser-supplied subscription key (`p256dh` / `auth`) is malformed.
    #[error("invalid subscription key: {0}")]
    InvalidSubscriptionKey(String),
    /// The payload exceeds what push services are required to accept.
    #[error(
        "push payload is {len} bytes after JSON encoding, but the maximum a push service must \
         accept leaves room for only {max}; shorten the title/body or move detail behind the url"
    )]
    PayloadTooLarge {
        /// Actual plaintext length.
        len: usize,
        /// Maximum permitted plaintext length.
        max: usize,
    },
    /// The message could not be serialized to JSON.
    #[error("could not serialize the push payload: {0}")]
    Serialization(String),
    /// Payload encryption failed.
    #[error("push payload encryption failed: {0}")]
    Encryption(String),
    /// The endpoint is already registered to a different principal, and the
    /// request did not present the key material that would prove it is the
    /// same user agent re-subscribing.
    #[error(
        "this push endpoint is already registered to a different account. Unsubscribe in the \
         browser first (`pushManager.getSubscription()` then `.unsubscribe()`), then subscribe \
         again."
    )]
    EndpointClaimed,
    /// The principal already holds the maximum number of subscriptions.
    #[error(
        "this account already has the maximum of {max} push subscriptions. Unsubscribe an \
         unused device before adding another."
    )]
    TooManySubscriptions {
        /// The per-principal ceiling.
        max: usize,
    },
    /// The subscription store failed.
    #[error("push subscription store error: {0}")]
    Store(String),
    /// No VAPID key is configured, so nothing can be signed or sent.
    #[error(
        "Web Push is not configured: no VAPID key. Set `[push] private_key = \"…\"` in \
         autumn.toml (mint one with `VapidKey::generate()`), or register a service carrying \
         one with `AppBuilder::with_web_push(...)`"
    )]
    NotConfigured,
    /// The `[push]` configuration block is internally inconsistent.
    #[error("invalid `[push]` configuration: {0}")]
    InvalidConfig(String),
    /// The transport could not reach the push service at all.
    #[error("push transport failed: {0}")]
    Transport(String),
}

impl PushError {
    /// The HTTP status this failure should surface as.
    ///
    /// Consulted by [`AutumnError`](crate::error::AutumnError)'s blanket
    /// conversion, so `push.subscribe(…).await?` in an application's own
    /// handler answers a malformed *browser* payload with the same `400` the
    /// built-in [`router`] does, instead of reporting a client mistake as a
    /// server fault.
    #[must_use]
    pub const fn status(&self) -> http::StatusCode {
        match self {
            // The caller sent something unusable.
            Self::InvalidEndpoint(_)
            | Self::InvalidSubscriptionKey(_)
            | Self::PayloadTooLarge { .. } => http::StatusCode::BAD_REQUEST,
            // Well-formed, but it conflicts with existing state…
            Self::EndpointClaimed => http::StatusCode::CONFLICT,
            // …or exceeds the caller's quota.
            Self::TooManySubscriptions { .. } => http::StatusCode::UNPROCESSABLE_ENTITY,
            // Push is not set up on this deployment: the request was fine, the
            // capability is absent. `503` invites a retry once it is
            // configured, where `500` would not.
            Self::NotConfigured => http::StatusCode::SERVICE_UNAVAILABLE,
            // Genuinely ours.
            Self::InvalidVapidKey(_)
            | Self::InvalidConfig(_)
            | Self::Serialization(_)
            | Self::Encryption(_)
            | Self::Store(_)
            | Self::Transport(_) => http::StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}
