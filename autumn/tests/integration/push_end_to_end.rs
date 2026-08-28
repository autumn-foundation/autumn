//! End-to-end Web Push (issue #1392): a browser subscribes over HTTP, the app
//! sends, and the exact bytes that would reach the push service are asserted —
//! then a `410 Gone` prunes the dead subscription.
//!
//! Everything here goes through the real, mounted routes and the real crypto.
//! The only substitution is the transport, which records requests instead of
//! putting them on the network; that is what makes it possible to assert the
//! VAPID `Authorization` header and the encrypted body at all.

use autumn_web::push::{
    MemoryPushSubscriptionStore, PushMessage, RecordingPushTransport, VapidKey, WebPush,
};
use autumn_web::test::TestApp;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;

/// The RFC 8291 §5 user agent — its private key lets this test decrypt what
/// the framework produced, proving a real browser could read it.
const UA_PRIVATE: &str = "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94";
const UA_PUBLIC: &str =
    "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
const UA_AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";

const DEAD_ENDPOINT: &str = "https://push.example.com/dead";
const LIVE_ENDPOINT: &str = "https://push.example.com/live";

fn subscription_json(endpoint: &str) -> serde_json::Value {
    json!({
        "endpoint": endpoint,
        "expirationTime": serde_json::Value::Null,
        "keys": { "p256dh": UA_PUBLIC, "auth": UA_AUTH },
    })
}

/// Decrypt an `aes128gcm` body the way the receiving browser does.
///
/// Deliberately written out longhand rather than reusing any framework code:
/// if the two ever disagree, this fails.
fn decrypt_as_the_browser_would(body: &[u8]) -> Vec<u8> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes128Gcm, Key, Nonce};
    use hmac::{Hmac, Mac};
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    use sha2::Sha256;

    fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
        type H = Hmac<Sha256>;
        let mut extract = <H as Mac>::new_from_slice(salt).expect("any key length");
        extract.update(ikm);
        let prk = extract.finalize().into_bytes();
        let mut expand = <H as Mac>::new_from_slice(&prk).expect("any key length");
        expand.update(info);
        expand.update(&[1_u8]);
        let mut okm = expand.finalize().into_bytes().to_vec();
        okm.truncate(len);
        okm
    }

    let salt = &body[..16];
    let id_len = body[20] as usize;
    let as_public = &body[21..21 + id_len];
    let ciphertext = &body[21 + id_len..];

    let ua_private =
        p256::SecretKey::from_slice(&URL_SAFE_NO_PAD.decode(UA_PRIVATE).expect("ua private key"))
            .expect("parses");
    let as_key = p256::PublicKey::from_sec1_bytes(as_public).expect("as public key parses");
    let shared = p256::ecdh::diffie_hellman(ua_private.to_nonzero_scalar(), as_key.as_affine());

    let ua_public_point = ua_private.public_key().to_encoded_point(false);
    let mut key_info = Vec::new();
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_public_point.as_bytes());
    key_info.extend_from_slice(as_public);

    let auth = URL_SAFE_NO_PAD.decode(UA_AUTH).expect("auth");
    let ikm = hkdf(&auth, shared.raw_secret_bytes(), &key_info, 32);
    let cek = hkdf(salt, &ikm, b"Content-Encoding: aes128gcm\0", 16);
    let nonce = hkdf(salt, &ikm, b"Content-Encoding: nonce\0", 12);

    let mut plaintext = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(&cek))
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: ciphertext,
                aad: b"",
            },
        )
        .expect("the browser must be able to decrypt what the framework sent");
    assert_eq!(
        plaintext.pop(),
        Some(0x02),
        "the last record carries the RFC 8188 delimiter"
    );
    plaintext
}

/// Verify a `vapid t=…, k=…` header the way a push service does, and return
/// the JWT's claims.
fn verify_vapid_header(header: &str, expected_key: &VapidKey) -> serde_json::Value {
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier;

    let (jwt, declared_key) = header
        .strip_prefix("vapid t=")
        .expect("RFC 8292 §3.1 single-header form")
        .split_once(", k=")
        .expect("`vapid t=…, k=…`");
    assert_eq!(
        declared_key,
        expected_key.public_key_base64url(),
        "`k=` must be the application server key the browser subscribed with"
    );

    let (signing_input, signature) = jwt.rsplit_once('.').expect("compact JWS");
    let signature = p256::ecdsa::Signature::from_slice(
        &URL_SAFE_NO_PAD
            .decode(signature)
            .expect("signature base64url"),
    )
    .expect("64-byte r‖s parses");
    VerifyingKey::from_sec1_bytes(&expected_key.public_key_bytes())
        .expect("public key parses")
        .verify(signing_input.as_bytes(), &signature)
        .expect("the push service must be able to verify this signature");

    let claims = URL_SAFE_NO_PAD
        .decode(signing_input.split('.').nth(1).expect("claims segment"))
        .expect("claims base64url");
    serde_json::from_slice(&claims).expect("claims are JSON")
}

/// The whole loop: subscribe over HTTP, send, and assert exactly what would
/// have gone to the push service.
#[tokio::test]
async fn subscribe_over_http_then_send_dispatches_a_signed_encrypted_request() {
    let transport = RecordingPushTransport::new();
    let vapid = VapidKey::generate();
    let push = WebPush::new(
        MemoryPushSubscriptionStore::new(),
        vapid.clone(),
        "mailto:ops@example.com",
        transport.clone(),
    );

    let client = TestApp::new()
        .merge(autumn_web::push::router())
        .with_web_push(push.clone())
        .build();
    client.acting_as(7).await;

    // ── Write path: the browser records its subscription ────────────────────
    client
        .post("/push/subscribe")
        .json(&subscription_json(LIVE_ENDPOINT))
        .send()
        .await
        .assert_status(204);

    // ── Read path: the app sends to that principal ──────────────────────────
    let report = push
        .send(
            7_i64,
            &PushMessage::new("Build failed", "main is red").url("/builds/42"),
        )
        .await
        .expect("send");
    assert_eq!(report.delivered, 1);
    assert!(report.pruned.is_empty());

    // ── Assert the dispatched request ───────────────────────────────────────
    let requests = transport.requests();
    assert_eq!(requests.len(), 1, "exactly one device is subscribed");
    let request = &requests[0];

    assert_eq!(
        request.endpoint, LIVE_ENDPOINT,
        "the message must go to the endpoint the browser recorded"
    );

    let claims = verify_vapid_header(
        request
            .header("authorization")
            .expect("a VAPID Authorization header"),
        &vapid,
    );
    assert_eq!(
        claims["aud"], "https://push.example.com",
        "aud must be the endpoint's origin"
    );
    assert_eq!(claims["sub"], "mailto:ops@example.com");

    assert_eq!(request.header("content-encoding"), Some("aes128gcm"));
    assert_eq!(
        request.header("content-type"),
        Some("application/octet-stream")
    );
    assert!(
        request.header("ttl").is_some(),
        "RFC 8030 §5.2 requires TTL"
    );

    // The body is genuinely encrypted for this subscription…
    assert!(
        !request
            .body
            .windows(b"Build failed".len())
            .any(|window| window == b"Build failed"),
        "the plaintext must never appear in the dispatched body"
    );
    // …and decrypts, in the browser, to what was sent.
    let message: serde_json::Value =
        serde_json::from_slice(&decrypt_as_the_browser_would(&request.body))
            .expect("the decrypted payload is the JSON the service worker reads");
    assert_eq!(message["title"], "Build failed");
    assert_eq!(message["body"], "main is red");
    assert_eq!(message["url"], "/builds/42");
}

/// A push service reporting `410 Gone` must remove the subscription — and the
/// next send must not dispatch to it again.
#[tokio::test]
async fn a_stale_endpoint_is_pruned_and_never_re_sent_to() {
    let transport = RecordingPushTransport::new().responding_with(DEAD_ENDPOINT, 410);
    let vapid = VapidKey::generate();
    let push = WebPush::new(
        MemoryPushSubscriptionStore::new(),
        vapid,
        "mailto:ops@example.com",
        transport.clone(),
    );

    let client = TestApp::new()
        .merge(autumn_web::push::router())
        .with_web_push(push.clone())
        .build();
    client.acting_as(7).await;

    for endpoint in [DEAD_ENDPOINT, LIVE_ENDPOINT] {
        client
            .post("/push/subscribe")
            .json(&subscription_json(endpoint))
            .send()
            .await
            .assert_status(204);
    }

    let first = push
        .send(7_i64, &PushMessage::new("Hi", "There"))
        .await
        .expect("send");
    assert_eq!(
        first.delivered, 1,
        "the live device still receives even though the other is dead"
    );
    assert_eq!(first.pruned, vec![DEAD_ENDPOINT.to_owned()]);

    let second = push
        .send(7_i64, &PushMessage::new("Hi", "again"))
        .await
        .expect("send");
    assert_eq!(second.delivered, 1);
    assert!(
        second.pruned.is_empty(),
        "the dead endpoint is already gone"
    );

    let dispatched_to_dead = transport
        .requests()
        .iter()
        .filter(|r| r.endpoint == DEAD_ENDPOINT)
        .count();
    assert_eq!(
        dispatched_to_dead, 1,
        "a pruned endpoint must never be re-sent to — that is the whole point"
    );
}

/// The composition documented for #1148: the in-app notification is the
/// durable record and is awaited; the push is the nudge and is best-effort.
#[tokio::test]
async fn a_push_failure_never_fails_the_in_app_notification_write() {
    use autumn_web::notifications::{MemoryNotificationStore, Notifications};
    use autumn_web::pagination::{ListQuery, PageRequest};

    let notifications = Notifications::new(MemoryNotificationStore::new());
    // No VAPID key configured: every send fails, which is the harshest version
    // of "push is broken".
    let push = WebPush::without_vapid_key(
        MemoryPushSubscriptionStore::new(),
        "mailto:ops@example.com",
        RecordingPushTransport::new(),
    );

    let notification = notifications
        .notify(7, "comment.created", json!({ "post": 42 }))
        .await
        .expect("the durable write must succeed");

    // Best-effort, exactly as the module docs show.
    let push_result = push
        .send(7_i64, &PushMessage::new("New comment", "Someone replied"))
        .await;
    assert!(push_result.is_err(), "push is genuinely broken here");

    let feed = notifications
        .list(7, &ListQuery::default(), &PageRequest::default())
        .await
        .expect("list");
    assert_eq!(
        feed.content.len(),
        1,
        "the in-app notification must survive a failed push"
    );
    assert_eq!(feed.content[0].id, notification.id);
    assert_eq!(
        notifications.unread_count(7).await.expect("unread"),
        1,
        "…and still count as unread, so the user sees it when they return"
    );
}

// ── Postgres-backed store (testcontainers, requires Docker) ─────────────────

/// `not(feature = "sqlite")`: under the app-only `sqlite` feature (which
/// implies `db`) the runtime connection is `SQLite`, so this Postgres pool would
/// no longer type-check against `DbPushSubscriptionStore::new`.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
mod pg {
    use super::*;
    use autumn_web::push::{DbPushSubscriptionStore, PushSubscriptionStore};
    use diesel_async::AsyncPgConnection;
    use diesel_async::RunQueryDsl;
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    /// The DDL `autumn generate pwa` emits for the Postgres backend.
    ///
    /// This is a **copy** of the generator's output, and copying it is the
    /// point: the generator lives in `autumn-cli`, which `autumn-web`'s tests
    /// cannot depend on, so the contract that binds them — the store's diesel
    /// `table!` must match the scaffolded table — is otherwise proved nowhere.
    /// Running the store against this DDL is what catches a column that was
    /// renamed on one side only, or a `TIMESTAMP` that should have been
    /// `TIMESTAMPTZ`. The `autumn-cli` test
    /// `generated_push_ddl_matches_the_ddl_the_framework_store_is_tested_against`
    /// pins the other direction, so the two cannot drift apart silently.
    const CREATE_PUSH_SUBSCRIPTIONS_SQL: &str = "CREATE TABLE push_subscriptions (\
         id BIGSERIAL PRIMARY KEY, \
         principal_id TEXT NOT NULL, \
         endpoint TEXT NOT NULL, \
         p256dh TEXT NOT NULL, \
         auth TEXT NOT NULL, \
         created_at TIMESTAMPTZ NOT NULL DEFAULT NOW())";
    const CREATE_PUSH_ENDPOINT_INDEX_SQL: &str = "CREATE UNIQUE INDEX idx_push_subscriptions_endpoint_unique \
         ON push_subscriptions (endpoint)";

    async fn setup() -> (
        DbPushSubscriptionStore,
        Pool<AsyncPgConnection>,
        testcontainers::ContainerAsync<Postgres>,
    ) {
        let container = Postgres::default()
            .start()
            .await
            .expect("failed to start postgres container");
        let host = container.get_host().await.expect("host");
        let port = container.get_host_port_ipv4(5432).await.expect("port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
        let pool = Pool::builder(manager).max_size(4).build().expect("pool");
        let mut conn = pool.get().await.expect("conn");
        for ddl in [
            CREATE_PUSH_SUBSCRIPTIONS_SQL,
            CREATE_PUSH_ENDPOINT_INDEX_SQL,
        ] {
            diesel::sql_query(ddl)
                .execute(&mut conn)
                .await
                .expect("apply the generated push DDL");
        }
        drop(conn);

        (DbPushSubscriptionStore::new(pool.clone()), pool, container)
    }

    /// Build a validated subscription without going through HTTP.
    fn stored(principal: i64, endpoint: &str) -> autumn_web::push::StoredSubscription {
        let payload: autumn_web::push::BrowserSubscription =
            serde_json::from_value(subscription_json(endpoint)).expect("payload");
        payload.decode(&principal.into()).expect("valid")
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn db_store_round_trips_through_the_generated_schema() {
        let (store, _pool, _container) = setup().await;

        store
            .save(stored(7, LIVE_ENDPOINT))
            .await
            .expect("the store must work against the DDL the generator emits");

        let rows = store.list_for("7").await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].endpoint(), LIVE_ENDPOINT);
        assert_eq!(rows[0].principal_id(), "7");
        // The keys must survive the TEXT round trip byte-for-byte, or nothing
        // this store hands back will encrypt correctly.
        assert_eq!(
            rows[0].p256dh(),
            URL_SAFE_NO_PAD
                .decode(UA_PUBLIC)
                .expect("decode")
                .as_slice()
        );
        assert_eq!(
            rows[0].auth(),
            URL_SAFE_NO_PAD.decode(UA_AUTH).expect("decode").as_slice()
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn db_store_upserts_on_endpoint_rather_than_duplicating() {
        let (store, _pool, _container) = setup().await;

        for _ in 0..3 {
            store.save(stored(7, LIVE_ENDPOINT)).await.expect("save");
        }
        assert_eq!(
            store.list_for("7").await.expect("list").len(),
            1,
            "the UNIQUE index on `endpoint` is what makes the upsert atomic; without \
             it this is three rows and three duplicate notifications"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn db_store_updates_the_keys_of_an_existing_endpoint() {
        // The `DO UPDATE SET` clause exists for key rotation. Re-saving an
        // identical row proves only "no second row"; this proves the columns
        // are actually refreshed.
        let (store, _pool, _container) = setup().await;
        store.save(stored(7, LIVE_ENDPOINT)).await.expect("save");

        let rotated_key = VapidKey::generate().public_key_base64url();
        let mut payload: autumn_web::push::BrowserSubscription =
            serde_json::from_value(subscription_json(LIVE_ENDPOINT)).expect("payload");
        payload.keys.p256dh = rotated_key.clone();
        let rotated = payload.decode(&7_i64.into()).expect("valid");
        store.save(rotated).await.expect("re-save with new keys");

        let rows = store.list_for("7").await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].p256dh_base64url(),
            rotated_key,
            "the stored key must be the rotated one, not the original"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn db_store_moves_an_endpoint_between_principals_only_with_matching_keys() {
        let (store, _pool, _container) = setup().await;
        store.save(stored(7, LIVE_ENDPOINT)).await.expect("save");

        // A different principal presenting DIFFERENT keys is an attempted
        // takeover: it must be refused, not silently honored.
        let mut payload: autumn_web::push::BrowserSubscription =
            serde_json::from_value(subscription_json(LIVE_ENDPOINT)).expect("payload");
        payload.keys.p256dh = VapidKey::generate().public_key_base64url();
        let hostile = payload.decode(&9_i64.into()).expect("valid shape");
        assert!(
            matches!(
                store.save(hostile).await,
                Err(autumn_web::push::PushError::EndpointClaimed)
            ),
            "knowing an endpoint URL must not be enough to take a device over"
        );
        assert_eq!(
            store.list_for("7").await.expect("list").len(),
            1,
            "the original owner keeps the subscription"
        );
        assert!(store.list_for("9").await.expect("list").is_empty());

        // The shared-device case: the SAME browser re-subscribes under a new
        // signed-in user, so it presents the same endpoint AND the same keys.
        store
            .save(stored(9, LIVE_ENDPOINT))
            .await
            .expect("a genuine shared-device re-subscribe is honored");
        assert!(store.list_for("7").await.expect("list").is_empty());
        assert_eq!(store.list_for("9").await.expect("list").len(), 1);
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn db_store_scopes_removal_by_principal() {
        let (store, _pool, _container) = setup().await;
        store.save(stored(7, LIVE_ENDPOINT)).await.expect("save");

        assert_eq!(
            store
                .remove(LIVE_ENDPOINT, Some("9"))
                .await
                .expect("remove"),
            0,
            "a different principal must not be able to delete this row"
        );
        assert_eq!(store.list_for("7").await.expect("list").len(), 1);

        // The unscoped path is what a `410 Gone` takes.
        assert_eq!(store.remove(LIVE_ENDPOINT, None).await.expect("remove"), 1);
        assert!(store.list_for("7").await.expect("list").is_empty());
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn db_store_caps_subscriptions_per_principal() {
        let (store, _pool, _container) = setup().await;
        let max = autumn_web::push::MAX_SUBSCRIPTIONS_PER_PRINCIPAL;

        for i in 0..max {
            store
                .save(stored(7, &format!("https://push.example.com/device-{i}")))
                .await
                .expect("save up to the cap");
        }
        assert!(
            matches!(
                store
                    .save(stored(7, "https://push.example.com/one-too-many"))
                    .await,
                Err(autumn_web::push::PushError::TooManySubscriptions { .. })
            ),
            "without a cap, one account can make every send unbounded work"
        );
        // An existing endpoint is an update, so the cap must not block it.
        store
            .save(stored(7, "https://push.example.com/device-0"))
            .await
            .expect("re-saving an existing endpoint is never capped");
        assert_eq!(store.list_for("7").await.expect("list").len(), max);
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn a_missing_table_reports_how_to_create_it() {
        let (store, pool, _container) = setup().await;
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query("DROP TABLE push_subscriptions")
            .execute(&mut conn)
            .await
            .expect("drop");
        drop(conn);

        let err = store
            .save(stored(7, LIVE_ENDPOINT))
            .await
            .expect_err("a missing table is an error");
        assert!(
            err.to_string().contains("autumn generate pwa"),
            "the error must say how to scaffold the table: {err}"
        );
    }
}
