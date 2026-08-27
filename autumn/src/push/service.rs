//! The Web Push send API: [`WebPush`].
//!
//! This is the surface an application actually touches. Everything
//! cryptographic — minting the VAPID JWT, deriving keys, encrypting the
//! payload — happens below it, so sending a notification is one call:
//!
//! ```rust,ignore
//! use autumn_web::push::{PushMessage, WebPush};
//! use autumn_web::prelude::*;
//!
//! #[post("/builds/{id}/fail")]
//! async fn build_failed(push: WebPush, id: Path<i64>) -> AutumnResult<&'static str> {
//!     push.send(
//!         owner_id,
//!         &PushMessage::new("Build failed", "main is red").url(format!("/builds/{}", *id)),
//!     )
//!     .await?;
//!     Ok("ok")
//! }
//! ```

use std::sync::Arc;

use futures::StreamExt as _;
use serde::{Deserialize, Serialize};

use super::PushError;
use super::encryption;
use super::store::{
    BoxedPushSubscriptionStore, BrowserSubscription, PushPrincipal, PushSubscriptionStore,
    StoredSubscription,
};
use super::transport::{PushRequest, PushTransport};
use super::vapid::VapidKey;
use crate::state::AppState;

/// Default `TTL` (RFC 8030 §5.2) on a dispatched message: how long the push
/// service may hold it for a device that is currently offline.
///
/// Four weeks matches what push services accept as an upper bound and suits
/// the re-engagement case this exists for — a notification is still worth
/// delivering when the user's laptop comes back online tomorrow.
pub const DEFAULT_TTL_SECS: u32 = 4 * 7 * 24 * 60 * 60;

/// How many of one principal's devices are dispatched to at once.
///
/// Push endpoints are client-chosen, so a device that accepts a connection and
/// never answers is a case to design for, not an accident: delivered serially
/// it would hold the whole send for the transport's timeout. Concurrency keeps
/// a stalled device from blocking its live siblings, and the bound keeps a
/// fan-out from opening one connection per subscription at once.
const MAX_CONCURRENT_DELIVERIES: usize = 8;

// ── Message ─────────────────────────────────────────────────────────────────

/// What the user sees: the payload delivered to the service worker's `push`
/// event.
///
/// Serialized as JSON, so the generated service worker can read
/// `event.data.json()` and hand `title`/`body`/`icon` straight to
/// `showNotification`, with `url` driving the `notificationclick` handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushMessage {
    /// The notification's title line.
    pub title: String,
    /// The notification's body text.
    pub body: String,
    /// Where clicking the notification takes the user. Omitted when unset, so
    /// the service worker's `notificationclick` handler can fall back to the
    /// app root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Icon URL shown alongside the notification. Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

impl PushMessage {
    /// A message with just a title and body.
    #[must_use]
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            url: None,
            icon: None,
        }
    }

    /// Set where clicking the notification navigates.
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Set the notification's icon URL.
    #[must_use]
    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }
}

// ── Delivery report ─────────────────────────────────────────────────────────

/// What one [`WebPush::send`] (or [`send_many`](WebPush::send_many)) actually
/// achieved.
///
/// A send never fails because one device is unreachable — a user with three
/// devices, one of which has revoked permission, still gets the notification
/// on the other two. This report is how that partial outcome is surfaced
/// rather than swallowed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushDeliveryReport {
    /// Subscriptions the push service accepted the message for.
    pub delivered: usize,
    /// Endpoints the push service reported as gone (`404`/`410`); these have
    /// been removed from the store and will not be tried again.
    pub pruned: Vec<String>,
    /// Subscriptions that failed for a reason that is **not** staleness (a
    /// push service outage, a rate limit, a transport error). These are left
    /// in the store to be retried.
    pub failed: usize,
}

impl PushDeliveryReport {
    /// Fold `other` into this report.
    fn merge(&mut self, other: Self) {
        self.delivered += other.delivered;
        self.failed += other.failed;
        self.pruned.extend(other.pruned);
    }
}

// ── Service ─────────────────────────────────────────────────────────────────

/// The Web Push service.
///
/// Declare it as a handler parameter (it implements `FromRequestParts`, like
/// [`Notifications`](crate::notifications::Notifications)) or construct one
/// directly with [`WebPush::new`] in tests and background jobs.
///
/// # Store and key resolution
///
/// As a handler extractor the store resolves the same way the notification
/// feed's does — an explicitly registered store, else the database-backed
/// store when a pool is configured, else the in-memory one — and the VAPID key
/// comes from the `[push]` config block, validated once at boot.
#[derive(Clone)]
pub struct WebPush {
    store: Arc<dyn BoxedPushSubscriptionStore>,
    /// `None` when no VAPID key is configured. Sending then fails with
    /// [`PushError::NotConfigured`] — never silently succeeds.
    vapid: Option<Arc<VapidKey>>,
    subject: Arc<str>,
    transport: Arc<dyn PushTransport>,
    ttl_secs: u32,
    clock: Arc<dyn crate::time::ClockSource>,
}

impl WebPush {
    /// Build a service over an explicit store, key, and transport.
    ///
    /// `subject` is the VAPID `sub` claim: a `mailto:` or `https:` URL a push
    /// service operator can use to reach you about your traffic.
    #[must_use]
    pub fn new(
        store: impl PushSubscriptionStore,
        vapid: VapidKey,
        subject: impl Into<String>,
        transport: impl PushTransport,
    ) -> Self {
        Self {
            store: Arc::new(store),
            vapid: Some(Arc::new(vapid)),
            subject: subject.into().into(),
            transport: Arc::new(transport),
            ttl_secs: DEFAULT_TTL_SECS,
            clock: Arc::new(crate::time::SystemClock),
        }
    }

    /// A service with **no** VAPID key: subscriptions can still be recorded,
    /// but every send fails with [`PushError::NotConfigured`].
    ///
    /// This is what an app that has generated a PWA but not yet configured
    /// `[push]` gets. Recording subscriptions early is harmless and means the
    /// app starts delivering the moment a key is configured; sending, by
    /// contrast, must never look like it worked.
    #[must_use]
    pub fn without_vapid_key(
        store: impl PushSubscriptionStore,
        subject: impl Into<String>,
        transport: impl PushTransport,
    ) -> Self {
        Self {
            store: Arc::new(store),
            vapid: None,
            subject: subject.into().into(),
            transport: Arc::new(transport),
            ttl_secs: DEFAULT_TTL_SECS,
            clock: Arc::new(crate::time::SystemClock),
        }
    }

    /// Override the `TTL` sent to the push service (default
    /// [`DEFAULT_TTL_SECS`]).
    #[must_use]
    pub const fn with_ttl_secs(mut self, ttl_secs: u32) -> Self {
        self.ttl_secs = ttl_secs;
        self
    }

    /// Override the clock used for the VAPID JWT's `exp`.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn crate::time::ClockSource>) -> Self {
        self.clock = clock;
        self
    }

    /// The `applicationServerKey` a browser needs to subscribe.
    ///
    /// Serve this to the client (the built-in [`router()`](super::router) does,
    /// at `/push/vapid-public-key`). It is public key material — safe to
    /// expose to any visitor.
    ///
    /// # Errors
    ///
    /// [`PushError::NotConfigured`] when no VAPID key is configured.
    pub fn vapid_public_key(&self) -> Result<String, PushError> {
        self.vapid
            .as_ref()
            .map(|key| key.public_key_base64url())
            .ok_or(PushError::NotConfigured)
    }

    /// Validate a browser `PushSubscription` and record it for `principal`.
    ///
    /// Re-subscribing the same endpoint updates the existing row rather than
    /// creating a second one, so a browser that re-subscribes on every page
    /// load (which is the recommended client pattern) never accumulates
    /// duplicates.
    ///
    /// # Errors
    ///
    /// [`PushError::InvalidEndpoint`] / [`PushError::InvalidSubscriptionKey`]
    /// for a payload that fails validation, or [`PushError::Store`] when the
    /// store fails.
    pub async fn subscribe(
        &self,
        principal: impl Into<PushPrincipal> + Send,
        subscription: &BrowserSubscription,
    ) -> Result<StoredSubscription, PushError> {
        let stored = subscription.decode(&principal.into())?;
        self.store.boxed_save(stored.clone()).await?;
        Ok(stored)
    }

    /// Remove `principal`'s subscription for `endpoint`.
    ///
    /// Scoped to the caller: an endpoint belonging to a different principal is
    /// left untouched and reported as `false`, so a signed-in user can never
    /// unsubscribe someone else's device. Idempotent — an endpoint that was
    /// never subscribed is `Ok(false)`, not an error.
    ///
    /// # Errors
    ///
    /// [`PushError::Store`] when the store fails.
    pub async fn unsubscribe(
        &self,
        principal: impl Into<PushPrincipal> + Send,
        endpoint: &str,
    ) -> Result<bool, PushError> {
        let principal = principal.into();
        let removed = self
            .store
            .boxed_remove(endpoint.to_owned(), Some(principal.as_str().to_owned()))
            .await?;
        Ok(removed > 0)
    }

    /// Deliver `message` to every device `principal` has subscribed.
    ///
    /// A principal with no subscriptions is not an error — it just means the
    /// user never granted notification permission — and returns an empty
    /// report. Per-device failures are reported, not raised: see
    /// [`PushDeliveryReport`].
    ///
    /// # Errors
    ///
    /// - [`PushError::NotConfigured`] when no VAPID key is configured. This is
    ///   raised **before** anything is dispatched, so an unconfigured app
    ///   fails loudly rather than appearing to send.
    /// - [`PushError::PayloadTooLarge`] when the encoded message exceeds what
    ///   push services are required to accept — also raised before dispatch,
    ///   since it would fail identically for every device.
    /// - [`PushError::Store`] when the subscription store itself fails.
    pub async fn send(
        &self,
        principal: impl Into<PushPrincipal> + Send,
        message: &PushMessage,
    ) -> Result<PushDeliveryReport, PushError> {
        let principal = principal.into();
        // Fail fast, before touching the store: a missing key, an unusable
        // `sub`, or an oversize payload fails identically for every device, so
        // discovering it per-device would just be N copies of the same error.
        let vapid = self.vapid.as_ref().ok_or(PushError::NotConfigured)?;
        // Checked here, not in `new`: the constructor is infallible (and takes
        // `impl Into<String>`), so a service built by
        // `AppBuilder::with_web_push` — from a secrets manager, say — never
        // passes through the boot-time `[push]` validation. Without this an
        // invalid `sub` would sign every request and every push service would
        // refuse it, showing up only as `report.failed` with no cause named.
        self.validate_subject()?;
        let payload = Self::encode(message)?;

        let subscriptions = self
            .store
            .boxed_list_for(principal.as_str().to_owned())
            .await?;

        if subscriptions.is_empty() {
            // Usually benign (the user never granted permission), but it is
            // also what a principal-namespace mismatch looks like — the
            // subscribe route recorded `user:42` while the send asked for
            // `42`. Naming the id that was looked up is the quickest way to
            // spot that, and there is no other symptom.
            tracing::debug!(
                principal = %principal,
                "no push subscriptions for this principal; nothing dispatched"
            );
        }

        // Dispatched concurrently, with a bound.
        //
        // Serially, one device that accepts the connection and then never
        // answers holds the whole send for the transport's full timeout, and
        // `send_many` queues every later recipient behind it — a principal at
        // the subscription cap could stall one notification for minutes.
        // Since every endpoint is client-chosen, that is reachable on purpose.
        //
        // Bounded rather than unbounded: a fan-out is still someone's
        // connection pool, and `MAX_CONCURRENT_DELIVERIES` at a time is plenty
        // to keep a stalled device from blocking its live siblings.
        let mut report = PushDeliveryReport::default();
        let mut deliveries = futures::stream::iter(
            subscriptions
                .iter()
                .map(|subscription| self.deliver_one(vapid, subscription, &payload)),
        )
        .buffer_unordered(MAX_CONCURRENT_DELIVERIES);
        while let Some(outcome) = futures::StreamExt::next(&mut deliveries).await {
            report.merge(outcome?);
        }
        drop(deliveries);
        // Deterministic order regardless of which device answered first, so a
        // caller (and a test) can compare reports.
        report.pruned.sort();
        Ok(report)
    }

    /// [`send`](Self::send) to many principals, aggregating one report.
    ///
    /// Deduplication is *not* performed: passing the same principal twice
    /// sends twice. Principals with no subscriptions contribute nothing.
    ///
    /// # Errors
    ///
    /// As [`send`](Self::send).
    pub async fn send_many<P>(
        &self,
        principals: impl IntoIterator<Item = P> + Send,
        message: &PushMessage,
    ) -> Result<PushDeliveryReport, PushError>
    where
        P: Into<PushPrincipal> + Send,
    {
        let mut report = PushDeliveryReport::default();
        for principal in principals {
            report.merge(self.send(principal, message).await?);
        }
        Ok(report)
    }

    /// Check the `sub` claim against what RFC 8292 permits.
    ///
    /// # Errors
    ///
    /// [`PushError::InvalidConfig`] when the subject is neither a `mailto:`
    /// nor an `https:` URI.
    fn validate_subject(&self) -> Result<(), PushError> {
        if super::config::is_valid_vapid_subject(&self.subject) {
            return Ok(());
        }
        Err(PushError::InvalidConfig(format!(
            "the VAPID `sub` claim must be a `mailto:` or `https:` URI (RFC 8292 §2.1), got \
             `{}`. A push service may refuse every message signed with anything else.",
            self.subject
        )))
    }

    /// Serialize a message and check it against the payload ceiling.
    fn encode(message: &PushMessage) -> Result<Vec<u8>, PushError> {
        let payload =
            serde_json::to_vec(message).map_err(|e| PushError::Serialization(e.to_string()))?;
        if payload.len() > encryption::MAX_PLAINTEXT_LEN {
            return Err(PushError::PayloadTooLarge {
                len: payload.len(),
                max: encryption::MAX_PLAINTEXT_LEN,
            });
        }
        Ok(payload)
    }

    /// Encrypt, sign, dispatch, and interpret the result for one subscription.
    async fn deliver_one(
        &self,
        vapid: &VapidKey,
        subscription: &StoredSubscription,
        payload: &[u8],
    ) -> Result<PushDeliveryReport, PushError> {
        let request = match self.build_request(vapid, subscription, payload) {
            Ok(request) => request,
            Err(e) => {
                // A row that no longer encrypts (a key corrupted in storage,
                // an endpoint that is no longer a valid URL) can never
                // succeed, but it must not abort delivery to this principal's
                // other devices either.
                tracing::warn!(
                    endpoint.origin = %endpoint_origin(subscription.endpoint()),
                    error = %e,
                    "skipping a push subscription that could not be encoded"
                );
                return Ok(PushDeliveryReport {
                    failed: 1,
                    ..Default::default()
                });
            }
        };

        let status = match self.transport.deliver(&request).await {
            Ok(status) => status,
            Err(e) => {
                tracing::warn!(
                    endpoint.origin = %endpoint_origin(subscription.endpoint()),
                    error = %e,
                    "push transport failed"
                );
                return Ok(PushDeliveryReport {
                    failed: 1,
                    ..Default::default()
                });
            }
        };

        // RFC 8030 §7.3: `404 Not Found` and `410 Gone` are the push service
        // saying this subscription no longer exists. Every OTHER failure —
        // including a 5xx outage or a 429 rate limit — is transient, and
        // pruning on those would silently unsubscribe every user during an
        // incident, unrecoverable without them re-granting permission.
        match status {
            200..=299 => Ok(PushDeliveryReport {
                delivered: 1,
                ..Default::default()
            }),
            404 | 410 => {
                // Unscoped: the endpoint is dead for whoever owns it.
                //
                // A store failure here is NOT propagated. The message has
                // already been dispatched and answered; failing the whole send
                // now would discard the deliveries that already succeeded to
                // this principal's other devices and abort the ones still to
                // come — the exact opposite of "a dead device never blocks a
                // live one". The row simply survives to be pruned on the next
                // send.
                match self
                    .store
                    .boxed_remove(subscription.endpoint().to_owned(), None)
                    .await
                {
                    Ok(_) => {
                        tracing::debug!(
                            endpoint.origin = %endpoint_origin(subscription.endpoint()),
                            status,
                            "pruned a stale push subscription"
                        );
                        Ok(PushDeliveryReport {
                            pruned: vec![subscription.endpoint().to_owned()],
                            ..Default::default()
                        })
                    }
                    Err(e) => {
                        tracing::warn!(
                            endpoint.origin = %endpoint_origin(subscription.endpoint()),
                            error = %e,
                            "could not prune a stale push subscription; it will be retried"
                        );
                        Ok(PushDeliveryReport {
                            failed: 1,
                            ..Default::default()
                        })
                    }
                }
            }
            other => {
                tracing::warn!(
                    endpoint.origin = %endpoint_origin(subscription.endpoint()),
                    status = other,
                    "push service rejected a message; leaving the subscription in place"
                );
                Ok(PushDeliveryReport {
                    failed: 1,
                    ..Default::default()
                })
            }
        }
    }

    /// Build the encrypted, VAPID-signed request for one subscription.
    fn build_request(
        &self,
        vapid: &VapidKey,
        subscription: &StoredSubscription,
        payload: &[u8],
    ) -> Result<PushRequest, PushError> {
        let body = encryption::encrypt(payload, subscription.p256dh(), subscription.auth())?;
        let issued_at = u64::try_from(self.clock.now().timestamp()).unwrap_or(0);
        let authorization =
            vapid.authorization_header(subscription.endpoint(), &self.subject, issued_at)?;
        Ok(PushRequest {
            endpoint: subscription.endpoint().to_owned(),
            headers: vec![
                ("authorization".to_owned(), authorization),
                ("content-encoding".to_owned(), "aes128gcm".to_owned()),
                (
                    "content-type".to_owned(),
                    "application/octet-stream".to_owned(),
                ),
                ("ttl".to_owned(), self.ttl_secs.to_string()),
            ],
            body,
        })
    }
}

// ── Extractor ───────────────────────────────────────────────────────────────

impl WebPush {
    /// The service an app that registered nothing gets.
    ///
    /// Resolution mirrors [`Notifications`](crate::notifications::Notifications):
    /// the database-backed store when a pool is configured, the in-memory one
    /// otherwise. The VAPID key and subject come from the validated `[push]`
    /// config block.
    fn default_for(state: &AppState) -> Self {
        let config = state.config();
        // Already validated at boot by `AppBuilder`, so a key that fails to
        // load here means the config was replaced at runtime. Treating that as
        // "unconfigured" is the safe reading: sends then fail loudly with
        // `NotConfigured` rather than silently signing with a stale key.
        let vapid = config.push.load_vapid_key().unwrap_or_else(|e| {
            tracing::error!(error = %e, "the `[push]` VAPID key failed to load; sends will fail");
            None
        });
        // Validated at boot; if it somehow fails here the configuration was
        // replaced at runtime, and the documented default is the safe reading.
        let subject = config
            .push
            .validated_subject()
            .unwrap_or_else(|_| super::config::DEFAULT_VAPID_SUBJECT.to_owned());
        let ttl_secs = config.push.ttl_secs.unwrap_or(DEFAULT_TTL_SECS);

        let transport = Self::default_transport(state);

        #[cfg(feature = "db")]
        let store: Arc<dyn BoxedPushSubscriptionStore> =
            if let Some(pool) = crate::db::DbState::pool(state) {
                Arc::new(super::store::DbPushSubscriptionStore::new(pool.clone()))
            } else {
                Arc::new(super::store::MemoryPushSubscriptionStore::new())
            };
        #[cfg(not(feature = "db"))]
        let store: Arc<dyn BoxedPushSubscriptionStore> =
            Arc::new(super::store::MemoryPushSubscriptionStore::new());

        Self {
            store,
            vapid: vapid.map(Arc::new),
            subject: subject.into(),
            transport,
            ttl_secs,
            clock: state.clock_arc(),
        }
    }

    /// [`default_for`](Self::default_for) with an explicitly registered store.
    ///
    /// Used by
    /// [`AppBuilder::with_push_subscription_store`](crate::app::AppBuilder::with_push_subscription_store),
    /// so a custom store still picks up the app's configured key, subject,
    /// TTL, transport and clock.
    pub(crate) fn from_state_with_store(
        state: &AppState,
        store: impl PushSubscriptionStore,
    ) -> Self {
        Self {
            store: Arc::new(store),
            ..Self::default_for(state)
        }
    }

    /// The transport a resolved service uses: a real HTTP client when one can
    /// be built, otherwise a transport that reports the failure rather than
    /// pretending to deliver.
    fn default_transport(state: &AppState) -> Arc<dyn PushTransport> {
        #[cfg(feature = "http-client")]
        {
            Arc::new(super::transport::HttpPushTransport::new(
                crate::http_client::Client::from_state(state),
            ))
        }
        #[cfg(not(feature = "http-client"))]
        {
            let _ = state;
            Arc::new(super::transport::UnavailablePushTransport)
        }
    }
}

impl axum::extract::FromRequestParts<AppState> for WebPush {
    // Resolution always succeeds (a store fallback always exists, and a
    // missing key surfaces from `send`, not extraction) — the same contract
    // `Notifications` and `Session` have.
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        _parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Fast path: a service registered by `AppBuilder::with_web_push` /
        // `with_push_subscription_store`, or resolved by an earlier request.
        if let Some(existing) = state.extension::<Self>() {
            return Ok((*existing).clone());
        }

        // Resolve OUTSIDE `extension_or_insert_with`.
        //
        // That helper holds the extensions **write** lock while it calls its
        // closure, and `default_for` reads other extensions off the same state
        // (the shared outbound HTTP client, and the mock registry under test).
        // A `std::sync::RwLock` is not reentrant, so resolving inside the
        // closure takes a read lock on a lock this thread already holds for
        // writing — and the request hangs forever. `Notifications` can resolve
        // inline only because its closure touches nothing but the database
        // pool.
        //
        // Building before the lock costs at most a redundant construction when
        // two requests race; `extension_or_insert_with` re-checks under the
        // lock, so exactly one is stored and the loser is dropped.
        let resolved = Self::default_for(state);
        // `extension_or_insert_with` keeps the resolved service (and thus the
        // memory store's contents) stable across requests.
        Ok((*state.extension_or_insert_with::<Self>(|| resolved)).clone())
    }
}

/// The origin of an endpoint URL, for logging.
///
/// A full push endpoint URL is a **capability**: anyone holding one can send
/// to that device. Logs are copied, shipped, and retained far more widely than
/// the subscription table, so framework logs record only the origin — enough
/// to tell FCM apart from Mozilla autopush when diagnosing a delivery problem,
/// and useless to anyone who reads the log.
fn endpoint_origin(endpoint: &str) -> String {
    url::Url::parse(endpoint).map_or_else(
        |_| "<unparseable>".to_owned(),
        |parsed| parsed.origin().ascii_serialization(),
    )
}

impl std::fmt::Debug for WebPush {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebPush")
            .field("configured", &self.vapid.is_some())
            .field("subject", &self.subject)
            .field("ttl_secs", &self.ttl_secs)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::encryption::hkdf_sha256;
    use crate::push::store::{MemoryPushSubscriptionStore, SubscriptionKeys};
    use crate::push::transport::RecordingPushTransport;
    use crate::push::vapid::decode_base64url;
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes128Gcm, Key, Nonce};
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use p256::elliptic_curve::sec1::ToEncodedPoint;

    // The RFC 8291 §5 user agent, whose PRIVATE key lets these tests decrypt
    // what the framework produced — proving the browser would really be able
    // to read it, not just that some bytes were sent.
    const UA_PRIVATE: &str = "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94";
    const UA_PUBLIC: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const UA_AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";

    fn browser_subscription(endpoint: &str) -> BrowserSubscription {
        BrowserSubscription {
            endpoint: endpoint.to_owned(),
            keys: SubscriptionKeys {
                p256dh: UA_PUBLIC.to_owned(),
                auth: UA_AUTH.to_owned(),
            },
        }
    }

    /// The receiving half of RFC 8291 — what a browser does with the body.
    ///
    /// Written out longhand (rather than reusing any of the sending code) so
    /// it is an independent check: if the two ever disagree, this fails.
    fn decrypt_as_the_browser_would(body: &[u8]) -> Vec<u8> {
        let salt = &body[..16];
        let id_len = body[20] as usize;
        let as_public = &body[21..21 + id_len];
        let ciphertext = &body[21 + id_len..];

        let ua_private =
            p256::SecretKey::from_slice(&decode_base64url(UA_PRIVATE).expect("ua private"))
                .expect("parses");
        let as_key = p256::PublicKey::from_sec1_bytes(as_public).expect("as public parses");
        let shared = p256::ecdh::diffie_hellman(ua_private.to_nonzero_scalar(), as_key.as_affine());

        let ua_public_bytes = ua_private.public_key().to_encoded_point(false);
        let mut key_info = Vec::new();
        key_info.extend_from_slice(b"WebPush: info\0");
        key_info.extend_from_slice(ua_public_bytes.as_bytes());
        key_info.extend_from_slice(as_public);

        let auth = decode_base64url(UA_AUTH).expect("auth");
        let ikm = hkdf_sha256(&auth, shared.raw_secret_bytes(), &key_info, 32);
        let cek = hkdf_sha256(salt, &ikm, b"Content-Encoding: aes128gcm\0", 16);
        let nonce = hkdf_sha256(salt, &ikm, b"Content-Encoding: nonce\0", 12);

        let cipher = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(&cek));
        let mut plaintext = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: ciphertext,
                    aad: b"",
                },
            )
            .expect("the browser must be able to decrypt what we sent");
        // Strip the RFC 8188 last-record delimiter.
        assert_eq!(plaintext.pop(), Some(0x02));
        plaintext
    }

    fn web_push_with(transport: RecordingPushTransport) -> (WebPush, VapidKey) {
        let vapid = VapidKey::generate();
        let push = WebPush::new(
            MemoryPushSubscriptionStore::new(),
            vapid.clone(),
            "mailto:ops@example.com",
            transport,
        );
        (push, vapid)
    }

    // ── PushMessage ─────────────────────────────────────────────────────────

    #[test]
    fn message_serializes_only_the_fields_that_are_set() {
        let json = serde_json::to_value(PushMessage::new("Hi", "There")).expect("serialize");
        assert_eq!(json["title"], "Hi");
        assert_eq!(json["body"], "There");
        assert!(
            json.get("url").is_none() && json.get("icon").is_none(),
            "absent options must not appear as nulls the service worker has to guard: {json}"
        );
    }

    #[test]
    fn message_builder_sets_url_and_icon() {
        let message = PushMessage::new("Hi", "There")
            .url("/inbox/42")
            .icon("/static/icons/icon.svg");
        assert_eq!(message.url.as_deref(), Some("/inbox/42"));
        assert_eq!(message.icon.as_deref(), Some("/static/icons/icon.svg"));
    }

    // ── Subscribe / unsubscribe ─────────────────────────────────────────────

    #[tokio::test]
    async fn subscribe_records_the_subscription_for_the_principal() {
        let (push, _) = web_push_with(RecordingPushTransport::new());
        let stored = push
            .subscribe(7_i64, &browser_subscription("https://push.example.com/a"))
            .await
            .expect("subscribe");
        assert_eq!(stored.principal_id(), "7");
        let report = push
            .send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");
        assert_eq!(
            report.delivered, 1,
            "the recorded subscription must receive"
        );
    }

    #[tokio::test]
    async fn subscribe_rejects_a_malformed_payload_rather_than_storing_it() {
        let (push, _) = web_push_with(RecordingPushTransport::new());
        let mut bad = browser_subscription("https://push.example.com/a");
        bad.keys.auth = URL_SAFE_NO_PAD.encode([0_u8; 4]);
        let err = push.subscribe(7_i64, &bad).await.expect_err("rejected");
        assert!(
            matches!(err, PushError::InvalidSubscriptionKey(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn unsubscribe_removes_only_the_callers_own_endpoint() {
        let (push, _) = web_push_with(RecordingPushTransport::new());
        push.subscribe(7_i64, &browser_subscription("https://push.example.com/a"))
            .await
            .expect("subscribe");

        assert!(
            !push
                .unsubscribe(8_i64, "https://push.example.com/a")
                .await
                .expect("unsubscribe"),
            "another principal must not be able to unsubscribe this device"
        );
        assert!(
            push.unsubscribe(7_i64, "https://push.example.com/a")
                .await
                .expect("unsubscribe"),
            "the owner unsubscribes successfully"
        );
        assert_eq!(
            push.send(7_i64, &PushMessage::new("Hi", "There"))
                .await
                .expect("send")
                .delivered,
            0
        );
    }

    #[tokio::test]
    async fn unsubscribing_an_unknown_endpoint_reports_false_not_an_error() {
        let (push, _) = web_push_with(RecordingPushTransport::new());
        assert!(
            !push
                .unsubscribe(7_i64, "https://push.example.com/never")
                .await
                .expect("unsubscribe is idempotent")
        );
    }

    // ── Send: the dispatched request ────────────────────────────────────────

    #[tokio::test]
    async fn send_dispatches_to_the_recorded_endpoint() {
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(7_i64, &browser_subscription("https://push.example.com/abc"))
            .await
            .expect("subscribe");

        let report = push
            .send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");

        assert_eq!(report.delivered, 1);
        assert!(report.pruned.is_empty());
        assert_eq!(report.failed, 0);
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].endpoint, "https://push.example.com/abc");
    }

    #[tokio::test]
    async fn dispatched_request_carries_a_verifiable_vapid_authorization_header() {
        use p256::ecdsa::VerifyingKey;
        use p256::ecdsa::signature::Verifier;

        let transport = RecordingPushTransport::new();
        let (push, vapid) = web_push_with(transport.clone());
        push.subscribe(7_i64, &browser_subscription("https://push.example.com/abc"))
            .await
            .expect("subscribe");
        push.send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");

        let requests = transport.requests();
        let authorization = requests[0]
            .header("authorization")
            .expect("every push request carries an Authorization header");
        assert!(authorization.starts_with("vapid t="), "{authorization}");

        let jwt = authorization
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split_once(", k="))
            .expect("`vapid t=…, k=…`")
            .0;
        let (signing_input, signature) = jwt.rsplit_once('.').expect("compact JWS");
        let signature = p256::ecdsa::Signature::from_slice(
            &URL_SAFE_NO_PAD
                .decode(signature)
                .expect("signature base64url"),
        )
        .expect("signature parses");
        VerifyingKey::from_sec1_bytes(&vapid.public_key_bytes())
            .expect("public key parses")
            .verify(signing_input.as_bytes(), &signature)
            .expect("the push service must be able to verify the signature");

        // The audience must be this endpoint's origin — a JWT minted for a
        // different push service is rejected by the real one.
        let claims = URL_SAFE_NO_PAD
            .decode(signing_input.split('.').nth(1).expect("claims"))
            .expect("claims base64url");
        let claims: serde_json::Value = serde_json::from_slice(&claims).expect("claims JSON");
        assert_eq!(claims["aud"], "https://push.example.com");
        assert_eq!(claims["sub"], "mailto:ops@example.com");
    }

    #[tokio::test]
    async fn dispatched_request_carries_the_aes128gcm_content_headers() {
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(7_i64, &browser_subscription("https://push.example.com/abc"))
            .await
            .expect("subscribe");
        push.send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");

        let requests = transport.requests();
        assert_eq!(requests[0].header("content-encoding"), Some("aes128gcm"));
        assert_eq!(
            requests[0].header("content-type"),
            Some("application/octet-stream")
        );
        let ttl = requests[0]
            .header("ttl")
            .expect("TTL is required by RFC 8030 §5.2");
        assert!(
            ttl.parse::<u32>().is_ok(),
            "TTL must be a plain number of seconds, got {ttl}"
        );
    }

    #[tokio::test]
    async fn dispatched_body_decrypts_to_the_message_in_the_browser() {
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(7_i64, &browser_subscription("https://push.example.com/abc"))
            .await
            .expect("subscribe");
        push.send(
            7_i64,
            &PushMessage::new("Build failed", "main is red").url("/builds/42"),
        )
        .await
        .expect("send");

        let requests = transport.requests();
        let plaintext = decrypt_as_the_browser_would(&requests[0].body);
        let message: serde_json::Value =
            serde_json::from_slice(&plaintext).expect("the payload is JSON the SW can read");
        assert_eq!(message["title"], "Build failed");
        assert_eq!(message["body"], "main is red");
        assert_eq!(message["url"], "/builds/42");
    }

    #[tokio::test]
    async fn each_device_gets_its_own_request() {
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(
            7_i64,
            &browser_subscription("https://push.example.com/laptop"),
        )
        .await
        .expect("subscribe");
        push.subscribe(
            7_i64,
            &browser_subscription("https://push.example.com/phone"),
        )
        .await
        .expect("subscribe");

        let report = push
            .send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");
        assert_eq!(report.delivered, 2);
        let mut endpoints: Vec<String> = transport
            .requests()
            .into_iter()
            .map(|r| r.endpoint)
            .collect();
        endpoints.sort();
        assert_eq!(
            endpoints,
            vec![
                "https://push.example.com/laptop".to_owned(),
                "https://push.example.com/phone".to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn every_message_gets_fresh_encryption_material() {
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(7_i64, &browser_subscription("https://push.example.com/abc"))
            .await
            .expect("subscribe");
        for _ in 0..2 {
            push.send(7_i64, &PushMessage::new("Hi", "There"))
                .await
                .expect("send");
        }
        let requests = transport.requests();
        assert_ne!(
            requests[0].body[..16],
            requests[1].body[..16],
            "reusing a salt across messages repeats an AES-GCM (key, nonce) pair"
        );
    }

    // ── Send: failure handling ──────────────────────────────────────────────

    #[tokio::test]
    async fn a_410_gone_prunes_the_subscription() {
        let transport =
            RecordingPushTransport::new().responding_with("https://push.example.com/dead", 410);
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(
            7_i64,
            &browser_subscription("https://push.example.com/dead"),
        )
        .await
        .expect("subscribe");

        let report = push
            .send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("a stale endpoint is not an error for the caller");
        assert_eq!(report.delivered, 0);
        assert_eq!(
            report.pruned,
            vec!["https://push.example.com/dead".to_owned()]
        );

        // The whole point of pruning: the next send must not dispatch again.
        let second = push
            .send(7_i64, &PushMessage::new("Hi", "again"))
            .await
            .expect("send");
        assert_eq!(second.delivered, 0);
        assert!(second.pruned.is_empty());
        assert_eq!(
            transport.requests().len(),
            1,
            "a pruned endpoint must never be re-sent to"
        );
    }

    #[tokio::test]
    async fn a_404_not_found_also_prunes() {
        let transport =
            RecordingPushTransport::new().responding_with("https://push.example.com/gone", 404);
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(
            7_i64,
            &browser_subscription("https://push.example.com/gone"),
        )
        .await
        .expect("subscribe");
        let report = push
            .send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");
        assert_eq!(
            report.pruned,
            vec!["https://push.example.com/gone".to_owned()]
        );
    }

    #[tokio::test]
    async fn a_transient_5xx_is_counted_but_never_prunes() {
        // Pruning on a 500 would silently unsubscribe every user during a push
        // service outage — unrecoverable without the user re-granting
        // permission.
        let transport =
            RecordingPushTransport::new().responding_with("https://push.example.com/a", 503);
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(7_i64, &browser_subscription("https://push.example.com/a"))
            .await
            .expect("subscribe");

        let report = push
            .send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");
        assert_eq!(report.failed, 1);
        assert!(
            report.pruned.is_empty(),
            "a transient failure must never unsubscribe a live device"
        );
        assert_eq!(
            push.send(7_i64, &PushMessage::new("Hi", "again"))
                .await
                .expect("send")
                .failed,
            1,
            "the subscription must still be there to retry"
        );
    }

    #[tokio::test]
    async fn one_dead_device_does_not_stop_delivery_to_the_others() {
        let transport =
            RecordingPushTransport::new().responding_with("https://push.example.com/dead", 410);
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(
            7_i64,
            &browser_subscription("https://push.example.com/dead"),
        )
        .await
        .expect("subscribe");
        push.subscribe(
            7_i64,
            &browser_subscription("https://push.example.com/live"),
        )
        .await
        .expect("subscribe");

        let report = push
            .send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");
        assert_eq!(report.delivered, 1);
        assert_eq!(report.pruned.len(), 1);
    }

    #[tokio::test]
    async fn sending_to_a_principal_with_no_subscriptions_is_a_no_op() {
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        let report = push
            .send(99_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("not an error — the user simply never granted permission");
        assert_eq!(report, PushDeliveryReport::default());
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn sending_without_a_vapid_key_fails_loudly_and_never_silently_succeeds() {
        let transport = RecordingPushTransport::new();
        let push = WebPush::without_vapid_key(
            MemoryPushSubscriptionStore::new(),
            "mailto:ops@example.com",
            transport.clone(),
        );
        push.subscribe(7_i64, &browser_subscription("https://push.example.com/a"))
            .await
            .expect("subscribe still works without a key");

        let err = push
            .send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect_err("an unconfigured push must be an error, never a silent no-op");
        assert!(matches!(err, PushError::NotConfigured), "{err:?}");
        assert!(
            err.to_string().contains("private_key"),
            "the error must say how to fix it: {err}"
        );
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn a_stalled_device_does_not_hold_up_its_live_siblings() {
        // Serially, one endpoint that accepts the connection and never answers
        // holds the whole send for the transport's full timeout. Every
        // endpoint is client-chosen, so this is reachable on purpose.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Blocks on the endpoint named `stall` until the others are done.
        #[derive(Debug)]
        struct StallingTransport {
            in_flight: Arc<AtomicUsize>,
        }

        impl PushTransport for StallingTransport {
            fn deliver<'a>(
                &'a self,
                request: &'a crate::push::PushRequest,
            ) -> crate::push::transport::PushTransportFuture<'a> {
                Box::pin(async move {
                    if request.endpoint.contains("stall") {
                        // Resolves only once every other device has been
                        // dispatched — impossible unless they run
                        // concurrently with this one.
                        for _ in 0..1_000 {
                            if self.in_flight.load(Ordering::SeqCst) >= 2 {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                        return Ok(201);
                    }
                    self.in_flight.fetch_add(1, Ordering::SeqCst);
                    Ok(201)
                })
            }
        }

        let in_flight = Arc::new(AtomicUsize::new(0));
        let push = WebPush::new(
            MemoryPushSubscriptionStore::new(),
            VapidKey::generate(),
            "mailto:ops@example.com",
            StallingTransport {
                in_flight: in_flight.clone(),
            },
        );
        for endpoint in [
            "https://push.example.com/stall",
            "https://push.example.com/live-a",
            "https://push.example.com/live-b",
        ] {
            push.subscribe(7_i64, &browser_subscription(endpoint))
                .await
                .expect("subscribe");
        }

        // Serial dispatch would deadlock here: the stalled device is first and
        // would wait forever for siblings that never start.
        let report = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            push.send(7_i64, &PushMessage::new("Hi", "There")),
        )
        .await
        .expect("a stalled device must not block the others")
        .expect("send");
        assert_eq!(report.delivered, 3);
    }

    #[tokio::test]
    async fn concurrent_delivery_still_prunes_and_reports_deterministically() {
        // Concurrency must not change what a send reports, only how fast it
        // gets there — including the order of `pruned`, which callers compare.
        let transport = RecordingPushTransport::new()
            .responding_with("https://push.example.com/dead-a", 410)
            .responding_with("https://push.example.com/dead-b", 404);
        let (push, _) = web_push_with(transport.clone());
        for endpoint in [
            "https://push.example.com/dead-b",
            "https://push.example.com/live",
            "https://push.example.com/dead-a",
        ] {
            push.subscribe(7_i64, &browser_subscription(endpoint))
                .await
                .expect("subscribe");
        }

        let report = push
            .send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");
        assert_eq!(report.delivered, 1);
        assert_eq!(
            report.pruned,
            vec![
                "https://push.example.com/dead-a".to_owned(),
                "https://push.example.com/dead-b".to_owned(),
            ],
            "pruned must be stably ordered whichever device answered first"
        );
        // And the pruning really happened.
        assert_eq!(
            push.send(7_i64, &PushMessage::new("Hi", "again"))
                .await
                .expect("send")
                .delivered,
            1
        );
    }

    #[tokio::test]
    async fn an_invalid_subject_is_refused_before_any_dispatch() {
        // `WebPush::new` is infallible and takes `impl Into<String>`, so a
        // service registered through `AppBuilder::with_web_push` never passes
        // through the boot-time `[push]` validation. Without this check an
        // invalid `sub` would sign every request and every push service would
        // refuse it, showing up only as `report.failed` with no cause named.
        let transport = RecordingPushTransport::new();
        let push = WebPush::new(
            MemoryPushSubscriptionStore::new(),
            VapidKey::generate(),
            // A bare address, not a `mailto:` URI — the common mistake.
            "ops@example.com",
            transport.clone(),
        );
        push.subscribe(7_i64, &browser_subscription("https://push.example.com/a"))
            .await
            .expect("subscribe");

        let err = push
            .send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect_err("an unusable sub must be refused");
        assert!(matches!(err, PushError::InvalidConfig(_)), "{err:?}");
        assert!(
            transport.requests().is_empty(),
            "nothing may be dispatched with a sub every push service will refuse"
        );
    }

    #[tokio::test]
    async fn a_valid_subject_still_sends() {
        for subject in ["mailto:ops@example.com", "https://example.com/contact"] {
            let transport = RecordingPushTransport::new();
            let push = WebPush::new(
                MemoryPushSubscriptionStore::new(),
                VapidKey::generate(),
                subject,
                transport.clone(),
            );
            push.subscribe(7_i64, &browser_subscription("https://push.example.com/a"))
                .await
                .expect("subscribe");
            assert_eq!(
                push.send(7_i64, &PushMessage::new("Hi", "There"))
                    .await
                    .unwrap_or_else(|e| panic!("{subject} must send, got {e}"))
                    .delivered,
                1
            );
        }
    }

    #[tokio::test]
    async fn vapid_public_key_is_the_browsers_application_server_key() {
        let (push, vapid) = web_push_with(RecordingPushTransport::new());
        assert_eq!(
            push.vapid_public_key().expect("configured"),
            vapid.public_key_base64url()
        );
    }

    #[tokio::test]
    async fn vapid_public_key_is_an_error_when_unconfigured() {
        let push = WebPush::without_vapid_key(
            MemoryPushSubscriptionStore::new(),
            "mailto:ops@example.com",
            RecordingPushTransport::new(),
        );
        assert!(matches!(
            push.vapid_public_key().expect_err("unconfigured"),
            PushError::NotConfigured
        ));
    }

    #[tokio::test]
    async fn an_oversize_message_is_refused_before_any_dispatch() {
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(7_i64, &browser_subscription("https://push.example.com/a"))
            .await
            .expect("subscribe");

        let err = push
            .send(7_i64, &PushMessage::new("Hi", "x".repeat(5000)))
            .await
            .expect_err("an oversize payload is refused");
        assert!(matches!(err, PushError::PayloadTooLarge { .. }), "{err:?}");
        assert!(
            transport.requests().is_empty(),
            "nothing may be dispatched when the payload cannot fit"
        );
    }

    // ── Fan-out ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn send_many_fans_out_across_principals() {
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(1_i64, &browser_subscription("https://push.example.com/one"))
            .await
            .expect("subscribe");
        push.subscribe(2_i64, &browser_subscription("https://push.example.com/two"))
            .await
            .expect("subscribe");
        push.subscribe(
            2_i64,
            &browser_subscription("https://push.example.com/two-b"),
        )
        .await
        .expect("subscribe");

        let report = push
            .send_many([1_i64, 2, 3], &PushMessage::new("Deploy", "shipped"))
            .await
            .expect("send_many");
        assert_eq!(report.delivered, 3, "3 has no subscription and is skipped");
        assert_eq!(transport.requests().len(), 3);
    }

    #[tokio::test]
    async fn send_many_aggregates_pruning_across_principals() {
        let transport =
            RecordingPushTransport::new().responding_with("https://push.example.com/dead", 410);
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(
            1_i64,
            &browser_subscription("https://push.example.com/dead"),
        )
        .await
        .expect("subscribe");
        push.subscribe(
            2_i64,
            &browser_subscription("https://push.example.com/live"),
        )
        .await
        .expect("subscribe");

        let report = push
            .send_many([1_i64, 2], &PushMessage::new("Deploy", "shipped"))
            .await
            .expect("send_many");
        assert_eq!(report.delivered, 1);
        assert_eq!(
            report.pruned,
            vec!["https://push.example.com/dead".to_owned()]
        );
    }

    #[tokio::test]
    async fn send_many_accepts_string_principals_too() {
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(
            "service:ci",
            &browser_subscription("https://push.example.com/ci"),
        )
        .await
        .expect("subscribe");
        assert_eq!(
            push.send_many(["service:ci"], &PushMessage::new("CI", "green"))
                .await
                .expect("send_many")
                .delivered,
            1
        );
    }

    // ── TTL and clock ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn ttl_defaults_to_the_documented_value() {
        // `ttl.parse().is_ok()` would pass for `0`, which means
        // deliver-now-or-drop — a real behavioural difference for the offline
        // device this feature exists to reach.
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        push.subscribe(7_i64, &browser_subscription("https://push.example.com/a"))
            .await
            .expect("subscribe");
        push.send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");
        assert_eq!(
            transport.requests()[0].header("ttl"),
            Some(DEFAULT_TTL_SECS.to_string().as_str())
        );
    }

    #[tokio::test]
    async fn a_configured_ttl_reaches_the_dispatched_header() {
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        let push = push.with_ttl_secs(60);
        push.subscribe(7_i64, &browser_subscription("https://push.example.com/a"))
            .await
            .expect("subscribe");
        push.send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");
        assert_eq!(transport.requests()[0].header("ttl"), Some("60"));
    }

    #[tokio::test]
    async fn the_jwt_exp_is_anchored_to_the_frameworks_clock() {
        // A broken clock (or the `unwrap_or(0)` in `build_request` firing)
        // would put `exp` in 1970 and make every real send be rejected by the
        // push service — with signature verification still passing, so no
        // other test would notice.
        use chrono::{TimeZone as _, Utc};

        let issued = Utc.timestamp_opt(1_800_000_000, 0).single().expect("time");
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        let push = push.with_clock(std::sync::Arc::new(crate::time::FixedClock::at(issued)));
        push.subscribe(7_i64, &browser_subscription("https://push.example.com/a"))
            .await
            .expect("subscribe");
        push.send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");

        let authorization = transport.requests()[0]
            .header("authorization")
            .expect("header")
            .to_owned();
        let jwt = authorization
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split_once(", k="))
            .expect("vapid header")
            .0
            .to_owned();
        let claims = URL_SAFE_NO_PAD
            .decode(jwt.split('.').nth(1).expect("claims"))
            .expect("base64url");
        let claims: serde_json::Value = serde_json::from_slice(&claims).expect("json");
        assert_eq!(
            claims["exp"].as_u64().expect("exp"),
            1_800_000_000 + crate::push::vapid::VAPID_TOKEN_TTL_SECS,
            "exp must be derived from the injected clock, not wall time or zero"
        );
    }

    // ── Store failures ──────────────────────────────────────────────────────

    /// A store whose every operation fails, for the paths
    /// [`MemoryPushSubscriptionStore`] can never reach.
    #[derive(Debug, Default)]
    struct BrokenStore;

    impl PushSubscriptionStore for BrokenStore {
        async fn save(&self, _subscription: StoredSubscription) -> Result<(), PushError> {
            Err(PushError::Store("save exploded".to_owned()))
        }
        async fn list_for(
            &self,
            _principal_id: &str,
        ) -> Result<Vec<StoredSubscription>, PushError> {
            Err(PushError::Store("list exploded".to_owned()))
        }
        async fn remove(
            &self,
            _endpoint: &str,
            _principal_id: Option<&str>,
        ) -> Result<u64, PushError> {
            Err(PushError::Store("remove exploded".to_owned()))
        }
    }

    /// A store that reads fine but cannot delete — the shape that makes a
    /// prune fail after the message has already been dispatched.
    #[derive(Debug)]
    struct UnprunableStore(MemoryPushSubscriptionStore);

    impl PushSubscriptionStore for UnprunableStore {
        async fn save(&self, subscription: StoredSubscription) -> Result<(), PushError> {
            self.0.save(subscription).await
        }
        async fn list_for(&self, principal_id: &str) -> Result<Vec<StoredSubscription>, PushError> {
            self.0.list_for(principal_id).await
        }
        async fn remove(
            &self,
            _endpoint: &str,
            _principal_id: Option<&str>,
        ) -> Result<u64, PushError> {
            Err(PushError::Store("remove exploded".to_owned()))
        }
    }

    #[tokio::test]
    async fn a_store_failure_on_read_surfaces_rather_than_reporting_zero_deliveries() {
        let push = WebPush::new(
            BrokenStore,
            VapidKey::generate(),
            "mailto:ops@example.com",
            RecordingPushTransport::new(),
        );
        let err = push
            .send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect_err("a broken store must not look like `no subscriptions`");
        assert!(matches!(err, PushError::Store(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_failed_prune_does_not_abort_delivery_to_the_other_devices() {
        // The prune happens AFTER dispatch. Propagating a store failure there
        // would discard the deliveries that already succeeded and skip the
        // ones still to come — the opposite of "a dead device never blocks a
        // live one".
        let transport =
            RecordingPushTransport::new().responding_with("https://push.example.com/dead", 410);
        let push = WebPush::new(
            UnprunableStore(MemoryPushSubscriptionStore::new()),
            VapidKey::generate(),
            "mailto:ops@example.com",
            transport.clone(),
        );
        push.subscribe(
            7_i64,
            &browser_subscription("https://push.example.com/dead"),
        )
        .await
        .expect("subscribe");
        push.subscribe(
            7_i64,
            &browser_subscription("https://push.example.com/live"),
        )
        .await
        .expect("subscribe");

        let report = push
            .send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("a prune failure must not fail the send");
        assert_eq!(
            report.delivered, 1,
            "the live device must still be reported as delivered"
        );
        assert!(
            report.pruned.is_empty(),
            "nothing was actually pruned, so nothing may be reported as pruned"
        );
        assert_eq!(report.failed, 1);
        assert_eq!(transport.requests().len(), 2, "both devices were attempted");
    }

    // ── Cross-origin fan-out ────────────────────────────────────────────────

    #[tokio::test]
    async fn each_endpoint_gets_a_jwt_audienced_to_its_own_push_service() {
        // Real deployments mix FCM, Mozilla autopush and WNS. A bug that
        // computed `aud` once — or reused one Authorization header across
        // devices — would pass every single-origin test and then be rejected
        // by every push service but the first.
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        for endpoint in [
            "https://fcm.googleapis.com/fcm/send/abc",
            "https://updates.push.services.mozilla.com/wpush/v2/xyz",
        ] {
            push.subscribe(7_i64, &browser_subscription(endpoint))
                .await
                .expect("subscribe");
        }
        push.send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");

        for request in transport.requests() {
            let authorization = request.header("authorization").expect("header");
            let jwt = authorization
                .strip_prefix("vapid t=")
                .and_then(|rest| rest.split_once(", k="))
                .expect("vapid header")
                .0;
            let claims = URL_SAFE_NO_PAD
                .decode(jwt.split('.').nth(1).expect("claims"))
                .expect("base64url");
            let claims: serde_json::Value = serde_json::from_slice(&claims).expect("json");
            let expected = url::Url::parse(&request.endpoint)
                .expect("endpoint parses")
                .origin()
                .ascii_serialization();
            assert_eq!(
                claims["aud"], expected,
                "each request's aud must be ITS OWN endpoint's origin"
            );
        }
    }

    #[tokio::test]
    async fn every_device_gets_its_own_encryption_material() {
        // One ephemeral key or salt reused across two subscriptions in the
        // same send would be an actual cryptographic break.
        let transport = RecordingPushTransport::new();
        let (push, _) = web_push_with(transport.clone());
        for endpoint in ["https://push.example.com/a", "https://push.example.com/b"] {
            push.subscribe(7_i64, &browser_subscription(endpoint))
                .await
                .expect("subscribe");
        }
        push.send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send");

        let requests = transport.requests();
        assert_ne!(requests[0].body[..16], requests[1].body[..16], "salt");
        assert_ne!(
            requests[0].body[21..86],
            requests[1].body[21..86],
            "ephemeral key"
        );
    }
}
