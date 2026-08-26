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
/// Only [`BrowserSubscription::decode`] produces one, so every value of this
/// type carries an https endpoint, a `p256dh` that is a real point on P-256,
/// and a 16-byte `auth` secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSubscription {
    /// Who this subscription delivers to.
    pub principal_id: String,
    /// The push service URL. Unique across the store: an endpoint identifies
    /// exactly one user agent installation.
    pub endpoint: String,
    /// The user agent's P-256 public key, raw (65 bytes, uncompressed).
    pub p256dh: Vec<u8>,
    /// The user agent's authentication secret, raw (16 bytes).
    pub auth: Vec<u8>,
}

impl StoredSubscription {
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
fn validate_endpoint(endpoint: &str) -> Result<String, PushError> {
    let endpoint = endpoint.trim();
    let parsed = url::Url::parse(endpoint)
        .map_err(|e| PushError::InvalidEndpoint(format!("{endpoint}: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(PushError::InvalidEndpoint(format!(
            "{endpoint}: push endpoints must use https, got `{}`",
            parsed.scheme()
        )));
    }
    match parsed.host() {
        Some(url::Host::Domain(host)) => {
            let host = host.to_ascii_lowercase();
            if host == "localhost" || host.ends_with(".localhost") {
                return Err(PushError::InvalidEndpoint(format!(
                    "{endpoint}: refusing a loopback push endpoint"
                )));
            }
        }
        Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) => {
            return Err(PushError::InvalidEndpoint(format!(
                "{endpoint}: push endpoints must name a host, not an IP literal"
            )));
        }
        None => {
            return Err(PushError::InvalidEndpoint(format!("{endpoint}: no host")));
        }
    }
    Ok(endpoint.to_owned())
}

// ── Store trait ─────────────────────────────────────────────────────────────

/// Pluggable storage for push subscriptions.
///
/// Implement this to persist subscriptions somewhere other than the built-in
/// backends. Two contracts every backend must honor:
///
/// - **`endpoint` is the primary identity.** [`save`](Self::save) upserts on
///   it: re-saving an endpoint updates the existing row (including moving it
///   to a different principal, which is what happens when a second user signs
///   in on a shared device) and never creates a second row.
/// - **Removal is idempotent.** [`remove`](Self::remove) returns how many rows
///   it deleted; a missing endpoint is `Ok(0)`, not an error.
pub trait PushSubscriptionStore: Send + Sync + 'static {
    /// Persist a subscription, replacing any existing row for the same
    /// endpoint.
    fn save(
        &self,
        subscription: StoredSubscription,
    ) -> impl Future<Output = Result<(), PushError>> + Send;

    /// Every subscription belonging to `principal_id` (empty when none do).
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
        // still receiving on it.
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
            let mut conn = self.conn().await?;
            let principal_id = subscription.principal_id.clone();
            let p256dh = subscription.p256dh_base64url();
            let auth = subscription.auth_base64url();
            diesel::insert_into(push_subscriptions::table)
                .values(NewSubscriptionRow {
                    principal_id: principal_id.clone(),
                    endpoint: subscription.endpoint.clone(),
                    p256dh: p256dh.clone(),
                    auth: auth.clone(),
                    created_at: Utc::now(),
                })
                // Endpoint identity: re-subscribing the same user agent MOVES
                // the row to whoever is signed in now (a shared device where a
                // second user signs in) instead of leaving the previous owner
                // still receiving on it.
                .on_conflict(push_subscriptions::endpoint)
                .do_update()
                .set((
                    push_subscriptions::principal_id.eq(principal_id),
                    push_subscriptions::p256dh.eq(p256dh),
                    push_subscriptions::auth.eq(auth),
                ))
                .execute(&mut conn)
                .await
                .map_err(|e| store_err(&e))?;
            Ok(())
        }

        async fn list_for(&self, principal_id: &str) -> Result<Vec<StoredSubscription>, PushError> {
            use push_subscriptions::dsl;
            let mut conn = self.conn().await?;
            let rows: Vec<SubscriptionRow> = dsl::push_subscriptions
                .filter(dsl::principal_id.eq(principal_id))
                .order(dsl::id.asc())
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

    fn browser_subscription(endpoint: &str) -> BrowserSubscription {
        BrowserSubscription {
            endpoint: endpoint.to_owned(),
            keys: SubscriptionKeys {
                p256dh: P256DH.to_owned(),
                auth: AUTH.to_owned(),
            },
        }
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
        assert_eq!(subscription.principal_id, "1");
        assert_eq!(subscription.endpoint, "https://push.example.com/a");
        assert_eq!(
            subscription.p256dh,
            URL_SAFE_NO_PAD.decode(P256DH).expect("decode")
        );
        assert_eq!(
            subscription.auth,
            URL_SAFE_NO_PAD.decode(AUTH).expect("decode")
        );
        assert_eq!(subscription.p256dh.len(), 65);
        assert_eq!(subscription.auth.len(), 16);
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
        assert_eq!(rows[0].endpoint, "https://push.example.com/a");
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
}
