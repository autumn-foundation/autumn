//! Origin-side glue between an Autumn app and the edge capsule (issue #1790).
//!
//! The edge lane's whole point is that one handler source serves from two
//! substrates. That only works if every seam it touches is mediated by the
//! framework, so this module supplies the origin half of the one seam the first
//! slice mediates: a key/value read.
//!
//! | Substrate | Behind [`EdgeCache`] sits… | Installed by |
//! | --- | --- | --- |
//! | Origin | [`CacheEdgeKv`] over the app's own `Cache` | [`AppBuilder::with_edge_kv`](crate::app::AppBuilder::with_edge_kv) |
//! | Edge | the capsule runtime's dialogue-backed reader | `autumn_edge::serve` |
//!
//! A handler sees neither: it takes [`EdgeCache`] and cannot tell which store
//! answered. That is what makes the identical source portable, and it is why
//! the adapter here reads through the *same* `insert_cached` / `get_cached`
//! serde path the rest of the framework writes through — an origin route that
//! caches bytes under a key has, by that act alone, published them to the edge
//! lane.
//!
//! # This is not a database (ADR-0004 category 2)
//!
//! [`EdgeKv`] is a replica-local, opportunistic read accelerator, never a
//! source of truth. It has no `put`; a miss is always a legal answer; staleness
//! is expected and there is no invalidation protocol. A route whose correctness
//! depends on the value being present, current, or authoritative does not
//! belong in the edge lane — serve it from the origin, where the database is.
//! [`CacheEdgeKv`] inherits exactly those properties from the cache it wraps.
//!
//! [`EdgeCache`]: autumn_edge::EdgeCache
//! [`EdgeKv`]: autumn_edge::EdgeKv

// This module runs on the request path: `EdgeKv::get` is called by the
// `EdgeCache` extractor while a request is in flight, so production code here
// must be panic-free. The deny block below IS compiled — and therefore enforced
// — by the `lint` job's `cargo clippy --workspace --all-targets -- -D warnings`:
// `examples/edge-greeting` is a workspace member whose native build enables
// `autumn-web/edge`, and cargo unifies features across the graph.
//
// It nevertheless carries no panic-gate marker comment and no
// `scripts/check-panic-gate.sh` manifest entry, on purpose. (Not even a
// *mention* of the marker tag: the gate's reverse-manifest scan greps for the
// literal tag text, so naming it here would enroll this module.) That script counts a
// non-default feature as linted only when an enforcing
// `cargo clippy -p autumn-web --features "…" -- -D warnings` lane names it, and
// none does (nor should one — the workspace lane already covers this module).
// So: the marker alone fails its reverse-manifest check, a manifest entry fails
// its feature-reachability check, and a `:default` suffix would be exactly the
// mislabelling that check exists to catch. Revisit only if `edge` ever gains a
// dedicated `-p autumn-web --features` clippy lane.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use autumn_edge::EdgeKv;

use crate::cache::Cache;

/// Opaque, validated identifier used while consulting an authoritative store.
/// It deliberately has no public accessor and is never part of [`EdgeIdentity`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(String);

/// Stable, normalized user identifier safe to send to an edge capsule.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct EdgeUserId(String);

impl EdgeUserId {
    /// Construct a normalized edge user id.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// Read the normalized value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A normalized authorization role safe to disclose to a capsule.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct EdgeRole(String);

impl EdgeRole {
    /// Construct a normalized role.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// Read the normalized role.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The complete and intentionally small identity envelope crossing the edge wire.
///
/// Its private fields make it structurally impossible to expose a cookie,
/// session id/signature, session map, signing secret, or backend key.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EdgeIdentity {
    user_id: EdgeUserId,
    roles: Vec<EdgeRole>,
}

impl EdgeIdentity {
    /// Construct an identity from normalized claims only.
    #[must_use]
    pub fn new(user_id: EdgeUserId, roles: Vec<EdgeRole>) -> Self {
        Self { user_id, roles }
    }
    /// Authenticated user claim.
    #[must_use]
    pub fn user_id(&self) -> &EdgeUserId {
        &self.user_id
    }
    /// Normalized role claims.
    #[must_use]
    pub fn roles(&self) -> &[EdgeRole] {
        &self.roles
    }
}

/// Host-only authentication contract for the Autumn edge integration.
///
/// `Ok(None)` covers both absent and invalid credentials. `Err` is reserved
/// for infrastructure failure so callers can fall through to the origin.
pub trait EdgeIdentityProvider: Send + Sync + 'static {
    /// Provider-specific infrastructure failure.
    type Error: std::error::Error + Send + Sync + 'static;
    /// Resolve an incoming host request without executing capsule code.
    fn resolve(
        &self,
        request: &http::Request<axum::body::Body>,
    ) -> impl Future<Output = Result<Option<EdgeIdentity>, Self::Error>> + Send;
}

/// Projects an authoritative session map into explicitly allowed edge claims.
pub trait EdgeIdentityProjector: Send + Sync + 'static {
    /// Return no identity when the session is not authenticated.
    fn project(&self, session: &HashMap<String, String>) -> Option<EdgeIdentity>;
}

/// Session-backed identity provider that shares Autumn's canonical cookie and
/// signing-key verification with [`crate::session::SessionLayer`].
pub struct SessionIdentityProvider<S, P> {
    store: S,
    projector: P,
    cookie_name: String,
    signing_keys: Arc<crate::security::config::ResolvedSigningKeys>,
}

impl<S, P> SessionIdentityProvider<S, P> {
    /// Build an adapter. The caller supplies the configured cookie name and the
    /// same resolved current/previous keys installed on `SessionLayer`.
    #[must_use]
    pub fn new(
        store: S,
        projector: P,
        cookie_name: impl Into<String>,
        signing_keys: Arc<crate::security::config::ResolvedSigningKeys>,
    ) -> Self {
        Self {
            store,
            projector,
            cookie_name: cookie_name.into(),
            signing_keys,
        }
    }
}

impl<S, P> EdgeIdentityProvider for SessionIdentityProvider<S, P>
where
    S: crate::session::SessionStore,
    P: EdgeIdentityProjector,
{
    type Error = crate::session::SessionStoreError;

    fn resolve(
        &self,
        request: &http::Request<axum::body::Body>,
    ) -> impl Future<Output = Result<Option<EdgeIdentity>, Self::Error>> + Send {
        let raw_id = crate::session::verified_session_id(
            request.headers(),
            &self.cookie_name,
            &self.signing_keys,
        );
        async move {
            let Some(raw_id) = raw_id else {
                return Ok(None);
            };
            let id = SessionId(raw_id);
            let Some(session) = self.store.load(&id.0).await? else {
                return Ok(None);
            };
            Ok(self.projector.project(&session))
        }
    }
}

/// Default projection using the application's configured `auth.session_key`.
pub struct AuthSessionProjector {
    auth_session_key: String,
}

impl AuthSessionProjector {
    /// Create a projector from `AutumnConfig::auth.session_key`.
    #[must_use]
    pub fn new(auth_session_key: impl Into<String>) -> Self {
        Self {
            auth_session_key: auth_session_key.into(),
        }
    }
}

impl EdgeIdentityProjector for AuthSessionProjector {
    fn project(&self, session: &HashMap<String, String>) -> Option<EdgeIdentity> {
        session
            .get(&self.auth_session_key)
            .map(|id| EdgeIdentity::new(EdgeUserId::new(id), Vec::new()))
    }
}

/// An [`EdgeKv`] backed by the application's own cache.
///
/// This is the adapter that makes an `#[edge(needs(kv))]` handler work at the
/// origin: it projects the seven-method, type-erased [`Cache`] onto the
/// one-method byte-oriented seam the edge lane can mediate.
///
/// # What the origin has to do to publish a value
///
/// Nothing edge-specific. Write bytes through the ordinary serde-aware cache
/// path and an edge handler reading the same key sees them:
///
/// ```rust
/// use std::sync::Arc;
///
/// use autumn_web::CacheEdgeKv;
/// use autumn_web::cache::{Cache, MokaCache, insert_cached};
/// use autumn_web::edge::EdgeKv;
///
/// let cache = MokaCache::new(128, None);
/// insert_cached(&cache, "banner", b"Autumn is up".to_vec(), None);
///
/// let kv = CacheEdgeKv::new(Arc::new(cache) as Arc<dyn Cache>);
/// assert_eq!(kv.get("banner"), Some(b"Autumn is up".to_vec()));
/// assert_eq!(kv.get("nothing-here"), None);
/// ```
///
/// `Vec<u8>` is the wire currency on purpose: it is the only shape that
/// survives both an in-process backend (stored as-is) and a serializing one
/// like Redis (JSON round-tripped), so a value published on one replica reads
/// back identically on another. A key holding some *other* type is reported as
/// a miss rather than an error — the seam has exactly one failure mode, and a
/// handler already has to render something sensible for it.
pub struct CacheEdgeKv(Arc<dyn Cache>);

impl CacheEdgeKv {
    /// Adapt a cache backend into the edge key/value seam.
    ///
    /// Pass the same backend the app serves from — typically the one given to
    /// [`AppBuilder::with_cache_backend`](crate::app::AppBuilder::with_cache_backend)
    /// — so the edge lane observes what the origin publishes.
    #[must_use]
    pub const fn new(cache: Arc<dyn Cache>) -> Self {
        Self(cache)
    }

    /// The cache this adapter reads through.
    #[must_use]
    pub fn cache(&self) -> &Arc<dyn Cache> {
        &self.0
    }
}

impl fmt::Debug for CacheEdgeKv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `dyn Cache` is not `Debug`, and a cache's contents are the last thing
        // that belongs in a log line anyway.
        f.write_str("CacheEdgeKv(..)")
    }
}

impl EdgeKv for CacheEdgeKv {
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        crate::cache::get_cached::<Vec<u8>>(self.0.as_ref(), key)
    }
}

#[cfg(all(test, feature = "cache-moka"))]
mod tests {
    use super::*;
    use crate::cache::{MokaCache, insert_cached};
    use crate::session::{MemoryStore, SessionStore};

    #[tokio::test]
    async fn session_identity_projects_only_normalized_claims() {
        let store = MemoryStore::new();
        let mut data = HashMap::new();
        data.insert("account_id".into(), "user-7".into());
        data.insert("backend_token".into(), "must-not-cross".into());
        store.save("sid-secret", data).await.expect("memory save");
        let keys = Arc::new(crate::security::config::ResolvedSigningKeys::new(
            b"current-secret".to_vec(),
            Vec::new(),
        ));
        let signature = keys.sign(b"sid-secret");
        let provider = SessionIdentityProvider::new(
            store,
            AuthSessionProjector::new("account_id"),
            "custom.sid",
            keys,
        );
        let request = http::Request::builder()
            .header(
                http::header::COOKIE,
                format!("custom.sid=sid-secret.{signature}"),
            )
            .body(axum::body::Body::empty())
            .expect("request");

        let identity = provider
            .resolve(&request)
            .await
            .expect("store available")
            .expect("authenticated");
        assert_eq!(identity.user_id().as_str(), "user-7");
        let wire = serde_json::to_string(&identity).expect("identity serializes");
        assert!(!wire.contains("sid-secret"));
        assert!(!wire.contains("backend_token"));
        assert!(!wire.contains("must-not-cross"));
    }

    #[tokio::test]
    async fn invalid_signature_and_destroyed_session_are_misses() {
        let store = MemoryStore::new();
        let keys = Arc::new(crate::security::config::ResolvedSigningKeys::new(
            b"secret".to_vec(),
            Vec::new(),
        ));
        let provider = SessionIdentityProvider::new(
            store,
            AuthSessionProjector::new("user_id"),
            "autumn.sid",
            keys,
        );
        let request = http::Request::builder()
            .header(http::header::COOKIE, "autumn.sid=gone.invalid")
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(
            provider.resolve(&request).await.expect("store available"),
            None
        );
    }

    fn cache_with(key: &str, value: &[u8]) -> Arc<dyn Cache> {
        let cache = MokaCache::new(16, None);
        insert_cached(&cache, key, value.to_vec(), None);
        Arc::new(cache)
    }

    #[test]
    fn reads_bytes_written_through_the_ordinary_cache_path() {
        let kv = CacheEdgeKv::new(cache_with("banner", b"hello"));
        assert_eq!(kv.get("banner"), Some(b"hello".to_vec()));
    }

    #[test]
    fn an_absent_key_is_a_miss() {
        let kv = CacheEdgeKv::new(cache_with("banner", b"hello"));
        assert_eq!(kv.get("absent"), None);
    }

    #[test]
    fn a_value_of_another_type_is_a_miss_not_a_failure() {
        let cache = MokaCache::new(16, None);
        insert_cached(&cache, "count", 7_u64, None);
        let kv = CacheEdgeKv::new(Arc::new(cache));

        assert_eq!(kv.get("count"), None);
    }

    #[test]
    fn a_serializing_backend_round_trips_through_the_raw_bytes_path() {
        // What a cross-replica backend (Redis) stores: JSON bytes under
        // `RawCacheBytes`, not the concrete `Vec<u8>`. The seam must read that
        // shape too, or a value published on one replica would vanish at the
        // edge of another.
        let cache = MokaCache::new(16, None);
        let json = serde_json::to_vec(&b"hello".to_vec()).expect("Vec<u8> serializes");
        cache.insert_value("banner", Arc::new(crate::cache::RawCacheBytes(json)));
        let kv = CacheEdgeKv::new(Arc::new(cache));

        assert_eq!(kv.get("banner"), Some(b"hello".to_vec()));
    }

    #[test]
    fn debug_names_the_adapter_without_leaking_contents() {
        let kv = CacheEdgeKv::new(cache_with("secret", b"s3cr3t"));
        let rendered = format!("{kv:?}");

        assert!(rendered.contains("CacheEdgeKv"), "{rendered}");
        assert!(!rendered.contains("s3cr3t"), "{rendered}");
    }
}
