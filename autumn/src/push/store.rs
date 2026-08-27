//! Push subscription storage: the browser payload, its validated form, and
//! the pluggable [`PushSubscriptionStore`] backends.
//!
//! A browser hands the application a `PushSubscription` — an endpoint URL plus
//! two base64url keys — which the app must persist against whoever is signed
//! in. [`BrowserSubscription::decode`] is the validating boundary: it is the
//! single place a browser-supplied (and therefore attacker-influenceable)
//! payload is turned into a [`StoredSubscription`], so a row that reaches a
//! store is always one the send path can actually use.
//!
//! # Backends
//!
//! Resolution mirrors [`Notifications`](crate::notifications::Notifications):
//!
//! 1. A store registered via `AppBuilder::with_push_subscription_store(...)`.
//! 2. [`DbPushSubscriptionStore`] when a database pool is configured (the
//!    `push_subscriptions` table is scaffolded by `autumn generate pwa`).
//! 3. [`MemoryPushSubscriptionStore`] otherwise — process-local, for tests
//!    and DB-less development.

use std::future::Future;
use std::pin::Pin;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use super::PushError;
use super::vapid::decode_base64url;

/// The `push_subscriptions` table name shared by [`DbPushSubscriptionStore`]
/// and the `autumn generate pwa` scaffolding.
pub const PUSH_SUBSCRIPTIONS_TABLE: &str = "push_subscriptions";

/// Length of the `auth` secret a browser publishes (RFC 8291 §3.2).
const AUTH_SECRET_LEN: usize = 16;

// ── Principal ───────────────────────────────────────────────────────────────

/// Whoever a subscription belongs to.
///
/// Stored as text so it fits both shapes Autumn apps use: the `i64` user id
/// the [`notifications`](crate::notifications) feed keys on, and the string
/// principal (`user:42`, `service:ci`) that
/// [`auth`](crate::auth) tokens carry. `PushPrincipal::from(42_i64)` and
/// `PushPrincipal::from("42")` are the same principal, so composing a push
/// with an in-app notification needs no conversion at the call site.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PushPrincipal(String);

impl PushPrincipal {
    /// The principal as it is stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<i64> for PushPrincipal {
    fn from(value: i64) -> Self {
        Self(value.to_string())
    }
}

impl From<&str> for PushPrincipal {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for PushPrincipal {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for PushPrincipal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── Browser payload ─────────────────────────────────────────────────────────

/// A browser `PushSubscription`, exactly as `JSON.stringify(subscription)`
/// serializes it.
///
/// This is the request body the built-in subscribe endpoint accepts. Fields
/// the browser adds and Autumn does not use (`expirationTime`) are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSubscription {
    /// The push service URL this user agent listens on.
    pub endpoint: String,
    /// The subscription's key material.
    pub keys: SubscriptionKeys,
}

/// The `keys` object of a browser `PushSubscription`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionKeys {
    /// The user agent's P-256 public key, base64url-encoded.
    pub p256dh: String,
    /// The user agent's 16-byte authentication secret, base64url-encoded.
    pub auth: String,
}

/// A subscription that has been validated and bound to a principal.
///
/// The fields are private and [`BrowserSubscription::decode`] is the only way
/// to build one, so every value of this type is *known* to carry a validated,
/// normalized https endpoint, a `p256dh` that is a real point on P-256, and a
/// 16-byte `auth` secret. A custom [`PushSubscriptionStore`] therefore never
/// has to re-validate what it is handed — and cannot be handed anything a
/// hostile client shaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSubscription {
    /// Who this subscription delivers to.
    principal_id: String,
    /// The push service URL, normalized. Unique across the store: an endpoint
    /// identifies exactly one user agent installation.
    endpoint: String,
    /// The user agent's P-256 public key, raw (65 bytes, uncompressed).
    p256dh: Vec<u8>,
    /// The user agent's authentication secret, raw (16 bytes).
    auth: Vec<u8>,
}

impl StoredSubscription {
    /// Who this subscription delivers to.
    #[must_use]
    pub fn principal_id(&self) -> &str {
        &self.principal_id
    }

    /// The push service URL, in the normalized form the store keys on.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The user agent's P-256 public key, raw (65 bytes, uncompressed).
    #[must_use]
    pub fn p256dh(&self) -> &[u8] {
        &self.p256dh
    }

    /// The user agent's authentication secret, raw (16 bytes).
    #[must_use]
    pub fn auth(&self) -> &[u8] {
        &self.auth
    }

    /// The `p256dh` key as the browser sent it.
    #[must_use]
    pub fn p256dh_base64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.p256dh)
    }

    /// The `auth` secret as the browser sent it.
    #[must_use]
    pub fn auth_base64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.auth)
    }
}

impl BrowserSubscription {
    /// Validate this payload and bind it to `principal`.
    ///
    /// # Errors
    ///
    /// - [`PushError::InvalidEndpoint`] — the endpoint is not an `https` URL,
    ///   or its host is one the framework refuses to make outbound requests to
    ///   (see the endpoint-safety note below).
    /// - [`PushError::InvalidSubscriptionKey`] — `p256dh` is not an
    ///   uncompressed P-256 point on the curve, or `auth` is not 16 bytes.
    ///
    /// # Endpoint safety
    ///
    /// The endpoint is a URL supplied by the client that the framework will
    /// later `POST` to, which makes an unchecked subscribe endpoint a
    /// server-side request forgery (SSRF) gadget. Two rules close it here, at
    /// the boundary, so a hostile row can never even be persisted:
    ///
    /// - the scheme must be `https`, so neither the encrypted body nor the
    ///   VAPID JWT that authenticates this application server ever crosses a
    ///   plaintext hop; and
    /// - the host must be a **domain name** that is not `localhost`. Every
    ///   real push service (FCM, Mozilla autopush, WNS) publishes a hostname,
    ///   so refusing IP literals outright is both stricter and simpler than
    ///   enumerating private ranges — `https://169.254.169.254/…`,
    ///   `https://10.0.0.1/…` and `https://[::1]/…` are all rejected by the
    ///   same rule, with no dependence on which Cargo features are enabled.
    ///
    /// A hostname that *resolves* to a private address is the remaining case;
    /// that one is caught at dispatch time by the SSRF-checked outbound client
    /// the default transport uses (see [`super::transport`]).
    pub fn decode(&self, principal: &PushPrincipal) -> Result<StoredSubscription, PushError> {
        let endpoint = validate_endpoint(&self.endpoint)?;

        let p256dh = decode_base64url(self.keys.p256dh.trim()).ok_or_else(|| {
            PushError::InvalidSubscriptionKey("`p256dh` is not valid base64url".to_owned())
        })?;
        // Parsing as a curve point rejects both a wrong length and a value
        // that merely looks like one, so no off-curve key reaches the ECDH.
        p256::PublicKey::from_sec1_bytes(&p256dh).map_err(|_| {
            PushError::InvalidSubscriptionKey(
                "`p256dh` is not an uncompressed P-256 public key on the curve".to_owned(),
            )
        })?;

        let auth = decode_base64url(self.keys.auth.trim()).ok_or_else(|| {
            PushError::InvalidSubscriptionKey("`auth` is not valid base64url".to_owned())
        })?;
        if auth.len() != AUTH_SECRET_LEN {
            return Err(PushError::InvalidSubscriptionKey(format!(
                "`auth` must be exactly {AUTH_SECRET_LEN} bytes, got {}",
                auth.len()
            )));
        }

        Ok(StoredSubscription {
            principal_id: principal.0.clone(),
            endpoint,
            p256dh,
            auth,
        })
    }
}

/// Check an endpoint URL and return it in normalized form.
///
/// See [`BrowserSubscription::decode`]'s "Endpoint safety" section for why
/// each rule is here.
/// The longest endpoint URL that will be stored.
///
/// Real push service endpoints are a few hundred bytes. A generous ceiling
/// stops a client filling the table — and every later `Vec<String>` of pruned
/// endpoints — with maximum-size rows.
pub(crate) const MAX_ENDPOINT_LEN: usize = 2048;

/// Check an endpoint URL and return it in **normalized** form.
///
/// See [`BrowserSubscription::decode`]'s "Endpoint safety" section for why
/// each rule is here.
///
/// Normalization is load-bearing, not cosmetic: `endpoint` is the store's
/// unique identity, so returning the raw client string would let
/// `https://x.example/p`, `https://x.example:443/p`, `https://X.example/p` and
/// `https://u:p@x.example/p` become four rows that all dispatch to the same
/// real endpoint — defeating both the upsert and the "re-subscribing never
/// duplicates" guarantee. `Url`'s own serialization lowercases the host, drops
/// the scheme's default port, and resolves dot segments; userinfo and fragment
/// are stripped here because neither is part of the endpoint's identity and a
/// push service never uses them.
fn validate_endpoint(endpoint: &str) -> Result<String, PushError> {
    let endpoint = endpoint.trim();
    if endpoint.len() > MAX_ENDPOINT_LEN {
        return Err(PushError::InvalidEndpoint(format!(
            "push endpoints must be at most {MAX_ENDPOINT_LEN} bytes, got {}",
            endpoint.len()
        )));
    }
    // The submitted URL is deliberately NOT echoed in these messages: the
    // built-in subscribe route surfaces them to the caller, and an endpoint
    // URL is a capability — anyone holding one can push to that device.
    let mut parsed = url::Url::parse(endpoint)
        .map_err(|e| PushError::InvalidEndpoint(format!("not a valid URL: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(PushError::InvalidEndpoint(format!(
            "push endpoints must use https, got `{}`",
            parsed.scheme()
        )));
    }
    match parsed.host() {
        Some(url::Host::Domain(host)) => {
            // `Url` lowercases the host but PRESERVES a trailing dot, and
            // `localhost.` is the fully-qualified form every resolver maps to
            // 127.0.0.1 — so the comparison has to strip it, or the loopback
            // rule is one character away from being bypassed.
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            if host.is_empty() || host == "localhost" || host.ends_with(".localhost") {
                return Err(PushError::InvalidEndpoint(
                    "refusing a loopback push endpoint".to_owned(),
                ));
            }
        }
        Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) => {
            return Err(PushError::InvalidEndpoint(
                "push endpoints must name a host, not an IP literal".to_owned(),
            ));
        }
        None => {
            return Err(PushError::InvalidEndpoint("no host".to_owned()));
        }
    }
    // Credentials and fragments are not part of the endpoint's identity, and a
    // push service never uses them; leaving them in would make one endpoint
    // several rows.
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_fragment(None);
    Ok(parsed.to_string())
}

// ── Store trait ─────────────────────────────────────────────────────────────

/// The most subscriptions one principal may hold.
///
/// Every device (and every browser profile) a user has is one subscription, so
/// the ceiling is generous — but it must exist: without it any authenticated
/// caller can register unbounded rows, and since a send dispatches to each in
/// turn, that turns one account into an unbounded amount of work per
/// notification.
///
/// # How the bound is actually guaranteed
///
/// Enforced in two places, deliberately, because they guarantee different
/// things:
///
/// - [`save`](PushSubscriptionStore::save) refuses a *new* endpoint past the
///   cap. This is a check-then-insert, so a burst of concurrent subscribes for
///   one principal can overshoot it by up to the number of requests in flight.
///   Serializing every subscribe per principal (an advisory lock, or
///   `SERIALIZABLE`) would close that window at a cost out of all proportion
///   to what it buys, because —
/// - [`list_for`](PushSubscriptionStore::list_for) returns **at most** this
///   many rows. That is the bound that matters: it caps the work a single
///   notification can cause no matter how many rows a race, a restored backup,
///   or a hand-written `INSERT` managed to leave in the table.
///
/// So the row count is approximately capped and the *per-notification work* is
/// strictly capped.
pub const MAX_SUBSCRIPTIONS_PER_PRINCIPAL: usize = 20;

/// Pluggable storage for push subscriptions.
///
/// Implement this to persist subscriptions somewhere other than the built-in
/// backends. Three contracts every backend must honor:
///
/// - **`endpoint` is the primary identity.** [`save`](Self::save) upserts on
///   it: re-saving an endpoint updates the existing row and never creates a
///   second one.
/// - **A cross-principal move requires proof of possession.** Re-saving an
///   endpoint that currently belongs to a *different* principal is allowed
///   only when the incoming `p256dh` **and** `auth` both match the stored ones;
///   otherwise it is
///   [`PushError::EndpointClaimed`]. This keeps the legitimate case working —
///   a shared device where a second user signs in re-subscribes and the
///   browser returns the *same* endpoint **and the same keys**, so the row
///   moves — while refusing the attack it otherwise enables: an endpoint URL
///   is only a capability to *send*, and without this rule anyone who obtained
///   one could re-register it under their own account with their own keys,
///   silently cutting the victim off and redirecting their notifications.
/// - **Removal is idempotent.** [`remove`](Self::remove) returns how many rows
///   it deleted; a missing endpoint is `Ok(0)`, not an error.
///
/// # Example
///
/// A store is registered once and then never mentioned again — the `WebPush`
/// extractor picks it up:
///
/// ```rust,ignore
/// use autumn_web::push::{PushError, PushSubscriptionStore, StoredSubscription};
///
/// struct RedisPushStore { /* … */ }
///
/// impl PushSubscriptionStore for RedisPushStore {
///     async fn save(&self, subscription: StoredSubscription) -> Result<(), PushError> {
///         // `subscription` is already validated: an https endpoint, a real
///         // P-256 point, a 16-byte auth secret. Key on `endpoint()`.
///         self.upsert(subscription.endpoint(), &subscription).await
///     }
///
///     async fn list_for(&self, principal_id: &str) -> Result<Vec<StoredSubscription>, PushError> {
///         self.by_principal(principal_id).await
///     }
///
///     async fn remove(
///         &self,
///         endpoint: &str,
///         principal_id: Option<&str>,
///     ) -> Result<u64, PushError> {
///         self.delete(endpoint, principal_id).await
///     }
/// }
///
/// autumn_web::app()
///     .with_push_subscription_store(RedisPushStore::new())
///     .merge(autumn_web::push::router())
///     .run()
///     .await;
/// ```
pub trait PushSubscriptionStore: Send + Sync + 'static {
    /// Persist a subscription, replacing any existing row for the same
    /// endpoint.
    ///
    /// Must return [`PushError::EndpointClaimed`] when the endpoint belongs to
    /// a different principal and the incoming `p256dh`/`auth` do not both
    /// match the stored ones, and [`PushError::TooManySubscriptions`] when the principal
    /// is already at [`MAX_SUBSCRIPTIONS_PER_PRINCIPAL`]. See the trait docs.
    fn save(
        &self,
        subscription: StoredSubscription,
    ) -> impl Future<Output = Result<(), PushError>> + Send;

    /// Every subscription belonging to `principal_id` (empty when none do).
    ///
    /// Must return at most [`MAX_SUBSCRIPTIONS_PER_PRINCIPAL`] rows. This is
    /// the hard bound on how much work one notification can cause — see that
    /// constant — so a backend must apply it even when the table somehow holds
    /// more.
    fn list_for(
        &self,
        principal_id: &str,
    ) -> impl Future<Output = Result<Vec<StoredSubscription>, PushError>> + Send;

    /// Delete the row for `endpoint`, returning how many rows were deleted.
    ///
    /// `principal_id` scopes the delete: `Some(id)` only removes the row when
    /// it belongs to that principal (the user-facing unsubscribe path, so one
    /// signed-in user can never drop another's device), while `None` removes
    /// it regardless of owner (the `404`/`410` pruning path, where the
    /// endpoint is dead for everyone).
    fn remove(
        &self,
        endpoint: &str,
        principal_id: Option<&str>,
    ) -> impl Future<Output = Result<u64, PushError>> + Send;
}

// ── Erasure bridge (same shape as notifications::BoxedNotificationStore) ─────
//
// `PushSubscriptionStore` uses RPIT futures and is not dyn-compatible, so the
// service holds a pub(crate) shadow trait with a blanket impl. Users only ever
// see `PushSubscriptionStore`.

type BoxedFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, PushError>> + Send + 'a>>;

pub(crate) trait BoxedPushSubscriptionStore: Send + Sync + 'static {
    fn boxed_save(&self, subscription: StoredSubscription) -> BoxedFuture<'_, ()>;
    fn boxed_list_for(&self, principal_id: String) -> BoxedFuture<'_, Vec<StoredSubscription>>;
    fn boxed_remove(&self, endpoint: String, principal_id: Option<String>) -> BoxedFuture<'_, u64>;
}

impl<S: PushSubscriptionStore> BoxedPushSubscriptionStore for S {
    fn boxed_save(&self, subscription: StoredSubscription) -> BoxedFuture<'_, ()> {
        Box::pin(PushSubscriptionStore::save(self, subscription))
    }

    fn boxed_list_for(&self, principal_id: String) -> BoxedFuture<'_, Vec<StoredSubscription>> {
        Box::pin(async move { PushSubscriptionStore::list_for(self, &principal_id).await })
    }

    fn boxed_remove(&self, endpoint: String, principal_id: Option<String>) -> BoxedFuture<'_, u64> {
        Box::pin(async move {
            PushSubscriptionStore::remove(self, &endpoint, principal_id.as_deref()).await
        })
    }
}

// ── In-memory store ─────────────────────────────────────────────────────────

/// Process-local [`PushSubscriptionStore`] used by default when no database is
/// configured.
///
/// Suitable for tests and DB-less development; contents are lost on restart
/// and it grows without bound (no eviction), so it is not a production store.
#[derive(Debug, Default)]
pub struct MemoryPushSubscriptionStore {
    rows: std::sync::Mutex<Vec<StoredSubscription>>,
}

impl MemoryPushSubscriptionStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Vec<StoredSubscription>>, PushError> {
        self.rows
            .lock()
            .map_err(|_| PushError::Store("memory store mutex poisoned".to_owned()))
    }
}

impl PushSubscriptionStore for MemoryPushSubscriptionStore {
    async fn save(&self, subscription: StoredSubscription) -> Result<(), PushError> {
        let mut rows = self.lock()?;

        // Endpoint identity, not (principal, endpoint): re-subscribing on a
        // shared device must MOVE the row, never leave the previous owner
        // still receiving on it. But a move across principals is only honored
        // when the caller presents BOTH stored keys — otherwise knowing an
        // endpoint URL and its (public) `p256dh` would be enough to take a
        // victim's device over, replacing `auth` in the process so the
        // victim's browser can no longer decrypt anything. See the trait docs.
        if let Some(existing) = rows
            .iter()
            .find(|row| row.endpoint == subscription.endpoint)
            && existing.principal_id != subscription.principal_id
            && (existing.p256dh != subscription.p256dh || existing.auth != subscription.auth)
        {
            return Err(PushError::EndpointClaimed);
        }

        let is_new = !rows.iter().any(|row| row.endpoint == subscription.endpoint);
        if is_new
            && rows
                .iter()
                .filter(|row| row.principal_id == subscription.principal_id)
                .count()
                >= MAX_SUBSCRIPTIONS_PER_PRINCIPAL
        {
            return Err(PushError::TooManySubscriptions {
                max: MAX_SUBSCRIPTIONS_PER_PRINCIPAL,
            });
        }

        rows.retain(|row| row.endpoint != subscription.endpoint);
        rows.push(subscription);
        drop(rows);
        Ok(())
    }

    async fn list_for(&self, principal_id: &str) -> Result<Vec<StoredSubscription>, PushError> {
        let rows = self.lock()?;
        Ok(rows
            .iter()
            .filter(|row| row.principal_id == principal_id)
            // The bound that actually caps per-notification work; see
            // `MAX_SUBSCRIPTIONS_PER_PRINCIPAL`.
            .take(MAX_SUBSCRIPTIONS_PER_PRINCIPAL)
            .cloned()
            .collect())
    }

    async fn remove(&self, endpoint: &str, principal_id: Option<&str>) -> Result<u64, PushError> {
        let mut rows = self.lock()?;
        let before = rows.len();
        rows.retain(|row| {
            row.endpoint != endpoint || principal_id.is_some_and(|id| row.principal_id != id)
        });
        Ok((before - rows.len()) as u64)
    }
}

// ── Database-backed store ───────────────────────────────────────────────────

#[cfg(feature = "db")]
pub use self::db_store::DbPushSubscriptionStore;

#[cfg(feature = "db")]
mod db_store {
    use super::{PUSH_SUBSCRIPTIONS_TABLE, PushError, PushSubscriptionStore, StoredSubscription};
    use crate::db::RuntimeConnection;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use chrono::{DateTime, Utc};
    use diesel::prelude::*;
    use diesel_async::RunQueryDsl;
    use diesel_async::pooled_connection::deadpool::Pool;

    // The `push_subscriptions` table as scaffolded by `autumn generate pwa`.
    // The two key columns are TEXT holding the browser's own base64url form
    // rather than BYTEA/BLOB: that keeps one `table!` definition working on
    // both backends (diesel's `Binary` maps to different SQL types per
    // backend) and makes the rows readable when debugging a delivery problem.
    #[cfg(not(feature = "sqlite"))]
    mod schema {
        diesel::table! {
            push_subscriptions (id) {
                id -> BigInt,
                principal_id -> Text,
                endpoint -> Text,
                p256dh -> Text,
                auth -> Text,
                created_at -> Timestamptz,
            }
        }
    }
    #[cfg(feature = "sqlite")]
    mod schema {
        diesel::table! {
            push_subscriptions (id) {
                id -> BigInt,
                principal_id -> Text,
                endpoint -> Text,
                p256dh -> Text,
                auth -> Text,
                created_at -> TimestamptzSqlite,
            }
        }
    }

    use schema::push_subscriptions;

    #[derive(Queryable, Selectable)]
    #[diesel(table_name = push_subscriptions)]
    struct SubscriptionRow {
        principal_id: String,
        endpoint: String,
        p256dh: String,
        auth: String,
    }

    #[derive(Insertable)]
    #[diesel(table_name = push_subscriptions)]
    struct NewSubscriptionRow {
        principal_id: String,
        endpoint: String,
        p256dh: String,
        auth: String,
        created_at: DateTime<Utc>,
    }

    impl TryFrom<SubscriptionRow> for StoredSubscription {
        type Error = PushError;

        fn try_from(row: SubscriptionRow) -> Result<Self, PushError> {
            let decode = |label: &str, value: &str| {
                URL_SAFE_NO_PAD.decode(value).map_err(|e| {
                    PushError::Store(format!(
                        "stored `{label}` for {} is not base64url: {e}",
                        row.endpoint
                    ))
                })
            };
            Ok(Self {
                p256dh: decode("p256dh", &row.p256dh)?,
                auth: decode("auth", &row.auth)?,
                principal_id: row.principal_id,
                endpoint: row.endpoint,
            })
        }
    }

    /// [`PushSubscriptionStore`] backed by the app's database pool.
    ///
    /// Expects the `push_subscriptions` table scaffolded by
    /// `autumn generate pwa`, whose `endpoint` column carries a UNIQUE
    /// constraint — that constraint is what makes [`save`](Self::save)'s
    /// upsert atomic rather than a racy select-then-insert.
    #[derive(Clone)]
    pub struct DbPushSubscriptionStore {
        pool: Pool<RuntimeConnection>,
    }

    impl DbPushSubscriptionStore {
        /// Build a store over the given pool.
        #[must_use]
        pub const fn new(pool: Pool<RuntimeConnection>) -> Self {
            Self { pool }
        }

        async fn conn(
            &self,
        ) -> Result<diesel_async::pooled_connection::deadpool::Object<RuntimeConnection>, PushError>
        {
            self.pool
                .get()
                .await
                .map_err(|e| PushError::Store(format!("checkout failed: {e}")))
        }
    }

    fn store_err(e: &diesel::result::Error) -> PushError {
        let message = e.to_string();
        // A missing table means the app has a database but never scaffolded
        // the push tables — turn the bare SQL error into an actionable one.
        if message.contains("does not exist") || message.contains("no such table") {
            return PushError::Store(format!(
                "query failed: {e}. The `{PUSH_SUBSCRIPTIONS_TABLE}` table is missing — \
                 scaffold it with `autumn generate pwa`, then apply it with `autumn migrate`"
            ));
        }
        PushError::Store(format!("query failed: {e}"))
    }

    impl PushSubscriptionStore for DbPushSubscriptionStore {
        async fn save(&self, subscription: StoredSubscription) -> Result<(), PushError> {
            // `filter` on an `ON CONFLICT … DO UPDATE` (the proof-of-possession
            // predicate below) comes from `FilterDsl`, which the prelude does
            // not re-export for insert statements. Imported inside this fn
            // rather than at module scope so it cannot shadow `QueryDsl::filter`
            // on the ordinary selects in `list_for`/`remove`.
            use diesel::query_dsl::methods::FilterDsl as _;
            use push_subscriptions::dsl;

            let mut conn = self.conn().await?;
            let principal_id = subscription.principal_id.clone();
            let endpoint = subscription.endpoint.clone();
            let p256dh = subscription.p256dh_base64url();
            let auth = subscription.auth_base64url();

            // Cap the principal's device count. A per-owner count is not a
            // constraint any single row can express, so this is a
            // check-then-insert and a concurrent burst can overshoot it — see
            // `MAX_SUBSCRIPTIONS_PER_PRINCIPAL` for why that is accepted
            // rather than serialized away (`list_for`'s LIMIT is the bound
            // that has to hold, and it does unconditionally). A pre-existing
            // endpoint is an update, so it is never blocked by the cap.
            // Fully qualified: `FilterDsl` is imported above for the upsert's
            // `DO UPDATE … WHERE`, which would otherwise make this ambiguous.
            let counted = diesel::QueryDsl::filter(
                diesel::QueryDsl::filter(
                    dsl::push_subscriptions,
                    dsl::principal_id.eq(&principal_id),
                ),
                dsl::endpoint.ne(&endpoint),
            );
            let existing: i64 = counted
                .count()
                .get_result(&mut conn)
                .await
                .map_err(|e| store_err(&e))?;
            if usize::try_from(existing).unwrap_or(usize::MAX)
                >= super::MAX_SUBSCRIPTIONS_PER_PRINCIPAL
            {
                return Err(PushError::TooManySubscriptions {
                    max: super::MAX_SUBSCRIPTIONS_PER_PRINCIPAL,
                });
            }

            let affected = diesel::insert_into(push_subscriptions::table)
                .values(NewSubscriptionRow {
                    principal_id: principal_id.clone(),
                    endpoint: endpoint.clone(),
                    p256dh: p256dh.clone(),
                    auth: auth.clone(),
                    created_at: Utc::now(),
                })
                // Endpoint identity: re-subscribing the same user agent
                // updates its row in place rather than adding a second.
                .on_conflict(dsl::endpoint)
                .do_update()
                .set((
                    dsl::principal_id.eq(&principal_id),
                    dsl::p256dh.eq(&p256dh),
                    dsl::auth.eq(&auth),
                ))
                // Proof of possession for a CROSS-principal move, enforced in
                // the same statement so it cannot race a concurrent subscribe:
                // the update only applies when the row already belongs to this
                // principal (an ordinary re-subscribe or key rotation), or when
                // the caller presents BOTH stored keys (the shared-device case,
                // where the browser returns the same endpoint AND the same
                // keys). `p256dh` alone is not enough: it is a *public* key, so
                // requiring `auth` too means learning the endpoint and its
                // public half still cannot take the row — which would cut the
                // victim off AND replace `auth`, leaving their browser unable
                // to decrypt. Anyone short of both fails and the statement
                // touches zero rows.
                .filter(
                    dsl::principal_id.eq(principal_id.clone()).or(dsl::p256dh
                        .eq(p256dh.clone())
                        .and(dsl::auth.eq(auth.clone()))),
                )
                .execute(&mut conn)
                .await
                .map_err(|e| store_err(&e))?;

            if affected == 0 {
                return Err(PushError::EndpointClaimed);
            }
            Ok(())
        }

        async fn list_for(&self, principal_id: &str) -> Result<Vec<StoredSubscription>, PushError> {
            use push_subscriptions::dsl;
            let mut conn = self.conn().await?;
            let rows: Vec<SubscriptionRow> = dsl::push_subscriptions
                .filter(dsl::principal_id.eq(principal_id))
                .order(dsl::id.asc())
                // The bound that actually caps per-notification work — applied
                // in SQL so it holds however many rows the table contains. See
                // `MAX_SUBSCRIPTIONS_PER_PRINCIPAL`.
                .limit(i64::try_from(super::MAX_SUBSCRIPTIONS_PER_PRINCIPAL).unwrap_or(i64::MAX))
                .select(SubscriptionRow::as_select())
                .load(&mut conn)
                .await
                .map_err(|e| store_err(&e))?;
            rows.into_iter().map(StoredSubscription::try_from).collect()
        }

        async fn remove(
            &self,
            endpoint: &str,
            principal_id: Option<&str>,
        ) -> Result<u64, PushError> {
            use push_subscriptions::dsl;
            let mut conn = self.conn().await?;
            let affected = match principal_id {
                Some(id) => {
                    diesel::delete(
                        dsl::push_subscriptions
                            .filter(dsl::endpoint.eq(endpoint))
                            .filter(dsl::principal_id.eq(id)),
                    )
                    .execute(&mut conn)
                    .await
                }
                None => {
                    diesel::delete(dsl::push_subscriptions.filter(dsl::endpoint.eq(endpoint)))
                        .execute(&mut conn)
                        .await
                }
            }
            .map_err(|e| store_err(&e))?;
            Ok(affected as u64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    const P256DH: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    /// A second, unrelated valid P-256 point — what an attacker (or a key
    /// rotation) would present.
    const OTHER_P256DH: &str =
        "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";

    fn browser_subscription(endpoint: &str) -> BrowserSubscription {
        BrowserSubscription {
            endpoint: endpoint.to_owned(),
            keys: SubscriptionKeys {
                p256dh: P256DH.to_owned(),
                auth: AUTH.to_owned(),
            },
        }
    }

    /// Put more rows in the store than [`MAX_SUBSCRIPTIONS_PER_PRINCIPAL`]
    /// allows, bypassing `save`'s cap the way a restored backup or a
    /// hand-written `INSERT` would.
    ///
    /// Deliberately not `async`: holding the guard inside an async fn would
    /// keep a `MutexGuard` alive across an await point.
    fn stuff_past_the_cap(store: &MemoryPushSubscriptionStore) {
        let extra: Vec<StoredSubscription> = (0..(MAX_SUBSCRIPTIONS_PER_PRINCIPAL * 3))
            .map(|i| stored(1_i64, &format!("https://push.example.com/raw{i}")))
            .collect();
        let mut rows = store.rows.lock().expect("memory store lock");
        rows.extend(extra);
        drop(rows);
    }

    fn stored(principal: impl Into<PushPrincipal>, endpoint: &str) -> StoredSubscription {
        browser_subscription(endpoint)
            .decode(&principal.into())
            .expect("valid subscription")
    }

    // ── PushPrincipal ───────────────────────────────────────────────────────

    #[test]
    fn principal_accepts_an_integer_user_id() {
        // Composing with the #1148 notification feed means reaching for the
        // same `recipient_id: i64` the feed uses, with no manual conversion.
        assert_eq!(PushPrincipal::from(42_i64).as_str(), "42");
    }

    #[test]
    fn principal_accepts_string_shaped_ids() {
        assert_eq!(PushPrincipal::from("user:42").as_str(), "user:42");
        assert_eq!(
            PushPrincipal::from("service:ci".to_owned()).as_str(),
            "service:ci"
        );
    }

    #[test]
    fn principal_for_the_same_user_id_is_stable_across_both_forms() {
        assert_eq!(PushPrincipal::from(7_i64), PushPrincipal::from("7"));
    }

    // ── Decoding the browser payload ────────────────────────────────────────

    #[test]
    fn decode_turns_base64url_keys_into_raw_bytes() {
        let subscription = stored(1_i64, "https://push.example.com/a");
        assert_eq!(subscription.principal_id(), "1");
        assert_eq!(subscription.endpoint(), "https://push.example.com/a");
        assert_eq!(
            subscription.p256dh(),
            URL_SAFE_NO_PAD.decode(P256DH).expect("decode")
        );
        assert_eq!(
            subscription.auth(),
            URL_SAFE_NO_PAD.decode(AUTH).expect("decode")
        );
        assert_eq!(subscription.p256dh().len(), 65);
        assert_eq!(subscription.auth().len(), 16);
    }

    #[test]
    fn decode_rejects_a_p256dh_that_is_not_a_curve_point() {
        // Validating at the STORE boundary means a hostile subscribe request
        // can never persist a row that only fails later, at send time.
        let mut sub = browser_subscription("https://push.example.com/a");
        let mut bytes = URL_SAFE_NO_PAD.decode(P256DH).expect("decode");
        bytes[64] ^= 0x01;
        sub.keys.p256dh = URL_SAFE_NO_PAD.encode(bytes);
        let err = sub
            .decode(&1_i64.into())
            .expect_err("an off-curve p256dh is rejected");
        assert!(
            matches!(err, PushError::InvalidSubscriptionKey(_)),
            "{err:?}"
        );
    }

    #[test]
    fn decode_rejects_a_wrong_length_auth_secret() {
        let mut sub = browser_subscription("https://push.example.com/a");
        sub.keys.auth = URL_SAFE_NO_PAD.encode([0_u8; 8]);
        let err = sub.decode(&1_i64.into()).expect_err("auth is 16 bytes");
        assert!(
            matches!(err, PushError::InvalidSubscriptionKey(_)),
            "{err:?}"
        );
    }

    #[test]
    fn decode_rejects_non_base64url_keys() {
        let mut sub = browser_subscription("https://push.example.com/a");
        sub.keys.p256dh = "not base64!!!".to_owned();
        let err = sub.decode(&1_i64.into()).expect_err("garbage is rejected");
        assert!(
            matches!(err, PushError::InvalidSubscriptionKey(_)),
            "{err:?}"
        );
    }

    #[test]
    fn decode_rejects_a_non_https_endpoint() {
        // A plaintext endpoint would ship the encrypted body — and the VAPID
        // JWT that authenticates us — over an interceptable channel.
        let err = browser_subscription("http://push.example.com/a")
            .decode(&1_i64.into())
            .expect_err("http:// endpoints are refused");
        assert!(matches!(err, PushError::InvalidEndpoint(_)), "{err:?}");
    }

    #[test]
    fn decode_rejects_an_endpoint_pointing_at_a_private_address() {
        // The endpoint is attacker-controlled input that the framework later
        // makes an outbound POST to: unchecked, subscribe is an SSRF gadget.
        for endpoint in [
            "https://127.0.0.1/push",
            "https://localhost/push",
            "https://10.0.0.1/push",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/push",
        ] {
            let err = browser_subscription(endpoint)
                .decode(&1_i64.into())
                .expect_err(&format!("{endpoint} must be refused"));
            assert!(
                matches!(err, PushError::InvalidEndpoint(_)),
                "{endpoint}: expected InvalidEndpoint, got {err:?}"
            );
        }
    }

    #[test]
    fn decode_accepts_a_real_push_service_endpoint() {
        for endpoint in [
            "https://fcm.googleapis.com/fcm/send/abc123",
            "https://updates.push.services.mozilla.com/wpush/v2/gAAAA",
            "https://wns2-par02p.notify.windows.com/w/?token=xyz",
        ] {
            browser_subscription(endpoint)
                .decode(&1_i64.into())
                .unwrap_or_else(|e| panic!("{endpoint} must be accepted, got {e}"));
        }
    }

    // ── MemoryPushSubscriptionStore ─────────────────────────────────────────

    #[tokio::test]
    async fn save_then_list_round_trips() {
        let store = MemoryPushSubscriptionStore::new();
        let subscription = stored(1_i64, "https://push.example.com/a");
        store.save(subscription.clone()).await.expect("save");
        assert_eq!(store.list_for("1").await.expect("list"), vec![subscription]);
    }

    #[tokio::test]
    async fn saving_the_same_endpoint_twice_upserts_rather_than_duplicates() {
        let store = MemoryPushSubscriptionStore::new();
        store
            .save(stored(1_i64, "https://push.example.com/a"))
            .await
            .expect("save");
        store
            .save(stored(1_i64, "https://push.example.com/a"))
            .await
            .expect("re-save");
        assert_eq!(
            store.list_for("1").await.expect("list").len(),
            1,
            "re-subscribing the same endpoint must update, not duplicate"
        );
    }

    #[tokio::test]
    async fn re_saving_an_endpoint_under_a_new_principal_moves_it() {
        // A shared device where a second user signs in re-subscribes the SAME
        // endpoint. Leaving the old row would deliver the first user's
        // notifications to the second.
        let store = MemoryPushSubscriptionStore::new();
        store
            .save(stored(1_i64, "https://push.example.com/a"))
            .await
            .expect("save");
        store
            .save(stored(2_i64, "https://push.example.com/a"))
            .await
            .expect("re-save under a new principal");
        assert!(
            store.list_for("1").await.expect("list").is_empty(),
            "the previous owner must no longer receive on this endpoint"
        );
        assert_eq!(store.list_for("2").await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn list_is_scoped_to_one_principal() {
        let store = MemoryPushSubscriptionStore::new();
        store
            .save(stored(1_i64, "https://push.example.com/a"))
            .await
            .expect("save");
        store
            .save(stored(2_i64, "https://push.example.com/b"))
            .await
            .expect("save");
        let rows = store.list_for("1").await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].endpoint(), "https://push.example.com/a");
    }

    #[tokio::test]
    async fn list_for_an_unknown_principal_is_empty_not_an_error() {
        let store = MemoryPushSubscriptionStore::new();
        assert!(store.list_for("nobody").await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn one_principal_may_hold_several_devices() {
        let store = MemoryPushSubscriptionStore::new();
        store
            .save(stored(1_i64, "https://push.example.com/laptop"))
            .await
            .expect("save");
        store
            .save(stored(1_i64, "https://push.example.com/phone"))
            .await
            .expect("save");
        assert_eq!(store.list_for("1").await.expect("list").len(), 2);
    }

    #[tokio::test]
    async fn unscoped_remove_prunes_by_endpoint() {
        // This is the path a `410 Gone` from the push service takes: the
        // endpoint is dead for everyone, so no principal scope applies.
        let store = MemoryPushSubscriptionStore::new();
        store
            .save(stored(1_i64, "https://push.example.com/a"))
            .await
            .expect("save");
        assert_eq!(
            store
                .remove("https://push.example.com/a", None)
                .await
                .expect("remove"),
            1
        );
        assert!(store.list_for("1").await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn scoped_remove_refuses_another_principals_endpoint() {
        // The unsubscribe route passes the caller's own principal, so one
        // signed-in user can never unsubscribe another user's device.
        let store = MemoryPushSubscriptionStore::new();
        store
            .save(stored(1_i64, "https://push.example.com/a"))
            .await
            .expect("save");
        assert_eq!(
            store
                .remove("https://push.example.com/a", Some("2"))
                .await
                .expect("remove"),
            0,
            "a different principal must not be able to delete this row"
        );
        assert_eq!(store.list_for("1").await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn removing_a_missing_endpoint_is_a_no_op_not_an_error() {
        let store = MemoryPushSubscriptionStore::new();
        assert_eq!(
            store
                .remove("https://push.example.com/gone", None)
                .await
                .expect("remove"),
            0
        );
    }
    // ── Endpoint hardening ──────────────────────────────────────────────────

    #[test]
    fn decode_rejects_a_trailing_dot_loopback_endpoint() {
        // `localhost.` is the fully-qualified form every resolver maps to
        // 127.0.0.1, and `Url` preserves the dot — so a check that compares
        // against `localhost` alone is one character from being bypassed.
        for endpoint in [
            "https://localhost./push",
            "https://LOCALHOST./push",
            "https://foo.localhost./push",
        ] {
            let err = browser_subscription(endpoint)
                .decode(&1_i64.into())
                .expect_err(&format!("{endpoint} must be refused"));
            assert!(
                matches!(err, PushError::InvalidEndpoint(_)),
                "{endpoint}: {err:?}"
            );
        }
    }

    #[test]
    fn decode_normalizes_the_endpoint_so_variants_are_one_row() {
        // `endpoint` is the store's unique identity. If these normalized to
        // different strings, one browser would become several rows dispatching
        // to the same real endpoint — several duplicate notifications, and the
        // upsert defeated.
        let canonical = stored(1_i64, "https://push.example.com/abc")
            .endpoint()
            .to_owned();
        for variant in [
            "https://push.example.com:443/abc",
            "https://PUSH.example.com/abc",
            "https://user:secret@push.example.com/abc",
            "https://push.example.com/abc#fragment",
            "  https://push.example.com/abc  ",
        ] {
            assert_eq!(
                stored(1_i64, variant).endpoint(),
                canonical,
                "{variant} must normalize to the same endpoint"
            );
        }
    }

    #[test]
    fn decode_does_not_echo_the_submitted_url_in_its_error() {
        // The built-in subscribe route surfaces these messages to the caller,
        // and an endpoint URL is a capability: anyone holding one can push to
        // that device.
        let secret = "https://push.example.com/SECRET-DEVICE-TOKEN";
        let mut sub = browser_subscription(secret);
        sub.endpoint = secret.replace("https", "http");
        let err = sub.decode(&1_i64.into()).expect_err("http is refused");
        assert!(
            !err.to_string().contains("SECRET-DEVICE-TOKEN"),
            "the error must not reflect the endpoint back: {err}"
        );
    }

    #[test]
    fn decode_rejects_an_absurdly_long_endpoint() {
        let long = format!("https://push.example.com/{}", "x".repeat(MAX_ENDPOINT_LEN));
        let err = browser_subscription(&long)
            .decode(&1_i64.into())
            .expect_err("oversize endpoints are refused");
        assert!(matches!(err, PushError::InvalidEndpoint(_)), "{err:?}");
    }

    // ── Takeover and capacity ───────────────────────────────────────────────

    #[tokio::test]
    async fn a_different_principal_cannot_claim_an_endpoint_with_its_own_keys() {
        // Knowing an endpoint URL must not be enough to take a device over:
        // that would silently cut the victim off AND redirect their
        // notifications to the attacker, who supplied the keys.
        let store = MemoryPushSubscriptionStore::new();
        store
            .save(stored(1_i64, "https://push.example.com/a"))
            .await
            .expect("save");

        let mut hostile = browser_subscription("https://push.example.com/a");
        hostile.keys.p256dh = OTHER_P256DH.to_owned();
        let hostile = hostile.decode(&2_i64.into()).expect("valid shape");

        assert!(
            matches!(store.save(hostile).await, Err(PushError::EndpointClaimed)),
            "an endpoint URL alone must not transfer a subscription"
        );
        assert_eq!(
            store.list_for("1").await.expect("list").len(),
            1,
            "the original owner keeps receiving"
        );
        assert!(store.list_for("2").await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn a_matching_p256dh_alone_does_not_transfer_an_endpoint() {
        // `p256dh` is a PUBLIC key. Accepting it alone as proof of possession
        // would let anyone who learned the endpoint and its public half take
        // the row — cutting the victim off AND replacing `auth`, so their
        // browser could no longer decrypt anything sent to it.
        let store = MemoryPushSubscriptionStore::new();
        store
            .save(stored(1_i64, "https://push.example.com/a"))
            .await
            .expect("save");

        let mut hostile = browser_subscription("https://push.example.com/a");
        // Correct public key, attacker's own auth secret.
        hostile.keys.auth = URL_SAFE_NO_PAD.encode([7_u8; 16]);
        let hostile = hostile.decode(&2_i64.into()).expect("valid shape");

        assert!(
            matches!(store.save(hostile).await, Err(PushError::EndpointClaimed)),
            "both keys must match for a cross-principal move"
        );
        let rows = store.list_for("1").await.expect("list");
        assert_eq!(rows.len(), 1, "the original owner keeps the subscription");
        assert_eq!(
            rows[0].auth(),
            URL_SAFE_NO_PAD.decode(AUTH).expect("decode").as_slice(),
            "and its auth secret is untouched, so it can still decrypt"
        );
    }

    #[tokio::test]
    async fn the_same_browser_re_subscribing_under_a_new_user_still_moves() {
        // The legitimate shared-device case: `pushManager.subscribe()` on an
        // unchanged registration returns the SAME endpoint and the SAME keys,
        // so possession is proved and the row moves.
        let store = MemoryPushSubscriptionStore::new();
        store
            .save(stored(1_i64, "https://push.example.com/a"))
            .await
            .expect("save");
        store
            .save(stored(2_i64, "https://push.example.com/a"))
            .await
            .expect("a genuine re-subscribe is honored");
        assert!(store.list_for("1").await.expect("list").is_empty());
        assert_eq!(store.list_for("2").await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn one_principal_may_rotate_its_own_keys_freely() {
        let store = MemoryPushSubscriptionStore::new();
        store
            .save(stored(1_i64, "https://push.example.com/a"))
            .await
            .expect("save");
        let mut rotated = browser_subscription("https://push.example.com/a");
        rotated.keys.p256dh = OTHER_P256DH.to_owned();
        store
            .save(rotated.decode(&1_i64.into()).expect("valid"))
            .await
            .expect("the owner may rotate its own keys");
        assert_eq!(store.list_for("1").await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn a_principal_cannot_register_unbounded_subscriptions() {
        // Every send dispatches to each subscription in turn, so an uncapped
        // principal turns one notification into unbounded work.
        let store = MemoryPushSubscriptionStore::new();
        for i in 0..MAX_SUBSCRIPTIONS_PER_PRINCIPAL {
            store
                .save(stored(1_i64, &format!("https://push.example.com/d{i}")))
                .await
                .expect("save up to the cap");
        }
        assert!(
            matches!(
                store
                    .save(stored(1_i64, "https://push.example.com/one-too-many"))
                    .await,
                Err(PushError::TooManySubscriptions { .. })
            ),
            "the cap must be enforced"
        );
        // An existing endpoint is an update, so it is never capped.
        store
            .save(stored(1_i64, "https://push.example.com/d0"))
            .await
            .expect("re-saving an existing endpoint is not capped");
        assert_eq!(
            store.list_for("1").await.expect("list").len(),
            MAX_SUBSCRIPTIONS_PER_PRINCIPAL
        );
    }

    #[tokio::test]
    async fn list_for_is_bounded_even_when_the_table_holds_more() {
        // This is the bound that actually caps per-notification work. The
        // insert-time cap is a check-then-insert and can be overshot by a
        // concurrent burst (or by a restored backup, or a hand-written
        // INSERT); this must hold regardless, so it is asserted against a
        // store deliberately stuffed past the cap.
        let store = MemoryPushSubscriptionStore::new();
        stuff_past_the_cap(&store);
        assert_eq!(
            store.list_for("1").await.expect("list").len(),
            MAX_SUBSCRIPTIONS_PER_PRINCIPAL,
            "one notification must never fan out past the cap"
        );
    }

    #[tokio::test]
    async fn the_cap_is_per_principal_not_global() {
        let store = MemoryPushSubscriptionStore::new();
        for i in 0..MAX_SUBSCRIPTIONS_PER_PRINCIPAL {
            store
                .save(stored(1_i64, &format!("https://push.example.com/d{i}")))
                .await
                .expect("save");
        }
        store
            .save(stored(2_i64, "https://push.example.com/other-user"))
            .await
            .expect("one user at their cap must not block everyone else");
    }
}
