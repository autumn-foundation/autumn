//! Integration tests for the first-class in-app notifications store
//! (issue #1148): per-recipient persistent feed with read/unread state,
//! surfaced through the [`Notifications`] service/extractor.
//!
//! Covers, per the acceptance criteria:
//! - notify → list / `unread_count` visibility (write path and read path),
//! - `unread_count` and `list` with the `unread` filter ignoring read rows,
//! - idempotent `mark_read` / `mark_all_read`,
//! - the `Notifications` extractor working end-to-end through `TestApp`
//!   (memory store fallback when no database is configured),
//! - best-effort realtime push over `channels` (a channel failure or absent
//!   subscriber never fails `notify`),
//! - the Postgres-backed store (testcontainers, **requires Docker**).

use autumn_web::AutumnResult;
use autumn_web::notifications::{MemoryNotificationStore, Notification, Notifications};
use autumn_web::pagination::{ListQuery, Page, PageRequest, SortDir};
use serde_json::json;

fn service() -> Notifications {
    Notifications::new(MemoryNotificationStore::new())
}

fn default_list() -> (ListQuery, PageRequest) {
    (ListQuery::default(), PageRequest::default())
}

// ── AC: notify is immediately visible to list / unread_count ──────────────

#[tokio::test]
async fn notify_creates_row_visible_to_list_and_unread_count() {
    let notifications = service();

    let created = notifications
        .notify(7, "comment.created", json!({"post": 42}))
        .await
        .expect("notify");
    assert_eq!(created.recipient_id, 7);
    assert_eq!(created.kind, "comment.created");
    assert_eq!(created.payload, json!({"post": 42}));
    assert!(created.read_at.is_none(), "new notification starts unread");

    let (query, page) = default_list();
    let feed: Page<Notification> = notifications.list(7, &query, &page).await.expect("list");
    assert_eq!(feed.total_elements, 1);
    assert_eq!(feed.content[0].id, created.id);

    assert_eq!(notifications.unread_count(7).await.expect("count"), 1);
    // Other recipients are unaffected.
    assert_eq!(notifications.unread_count(8).await.expect("count"), 0);
}

// ── AC: unread_count / list(unread) ignore read rows ──────────────────────

#[tokio::test]
async fn unread_count_and_unread_filter_ignore_read_notifications() {
    let notifications = service();

    let a = notifications
        .notify(1, "a", json!({}))
        .await
        .expect("notify a");
    let b = notifications
        .notify(1, "b", json!({}))
        .await
        .expect("notify b");

    notifications.mark_read(a.id).await.expect("mark_read");

    assert_eq!(notifications.unread_count(1).await.expect("count"), 1);

    let unread_query = ListQuery::new(None, SortDir::default(), &[("unread", "true")]);
    let page = PageRequest::default();
    let unread = notifications
        .list(1, &unread_query, &page)
        .await
        .expect("list unread");
    assert_eq!(unread.total_elements, 1);
    assert_eq!(unread.content[0].id, b.id);

    // The unfiltered feed still shows both.
    let (query, page) = default_list();
    let all = notifications
        .list(1, &query, &page)
        .await
        .expect("list all");
    assert_eq!(all.total_elements, 2);
}

// ── AC: mark_read / mark_all_read are idempotent ──────────────────────────

#[tokio::test]
async fn mark_read_is_idempotent() {
    let notifications = service();
    let n = notifications
        .notify(3, "x", json!({}))
        .await
        .expect("notify");

    notifications
        .mark_read(n.id)
        .await
        .expect("first mark_read");
    // Re-marking a read notification is a no-op, not an error.
    notifications
        .mark_read(n.id)
        .await
        .expect("second mark_read");
    assert_eq!(notifications.unread_count(3).await.expect("count"), 0);

    // A read_at timestamp is preserved, not overwritten, by the re-mark.
    let (query, page) = default_list();
    let feed = notifications.list(3, &query, &page).await.expect("list");
    assert!(feed.content[0].read_at.is_some());
}

#[tokio::test]
async fn mark_read_on_missing_id_is_a_noop() {
    let notifications = service();
    notifications
        .mark_read(999)
        .await
        .expect("missing id is a no-op");
}

#[tokio::test]
async fn mark_all_read_is_idempotent() {
    let notifications = service();
    notifications
        .notify(5, "a", json!({}))
        .await
        .expect("notify");
    notifications
        .notify(5, "b", json!({}))
        .await
        .expect("notify");

    let marked = notifications.mark_all_read(5).await.expect("mark_all_read");
    assert_eq!(marked, 2);
    assert_eq!(notifications.unread_count(5).await.expect("count"), 0);

    // Second call marks nothing and does not error.
    let marked_again = notifications
        .mark_all_read(5)
        .await
        .expect("second mark_all_read");
    assert_eq!(marked_again, 0);
}

// ── Recipient-scoped mark_read (IDOR-safe variant for handlers) ───────────

#[tokio::test]
async fn mark_read_for_only_touches_the_recipients_own_notification() {
    let notifications = service();
    let n = notifications
        .notify(1, "a", json!({}))
        .await
        .expect("notify");

    // A different recipient cannot mark someone else's notification read.
    notifications
        .mark_read_for(2, n.id)
        .await
        .expect("no-op for other recipient");
    assert_eq!(notifications.unread_count(1).await.expect("count"), 1);

    notifications
        .mark_read_for(1, n.id)
        .await
        .expect("owner marks read");
    assert_eq!(notifications.unread_count(1).await.expect("count"), 0);
}

// ── Feed ordering and pagination ──────────────────────────────────────────

#[tokio::test]
async fn list_returns_newest_first_and_paginates() {
    let notifications = service();
    for i in 0..5 {
        notifications
            .notify(9, format!("kind{i}"), json!({"i": i}))
            .await
            .expect("notify");
    }

    let query = ListQuery::default();
    let first = notifications
        .list(9, &query, &PageRequest::new(1, 2))
        .await
        .expect("page 1");
    assert_eq!(first.total_elements, 5);
    assert_eq!(first.content.len(), 2);
    assert!(first.has_next);
    assert!(!first.has_previous);
    // Newest first by default.
    assert_eq!(first.content[0].kind, "kind4");
    assert_eq!(first.content[1].kind, "kind3");

    let last = notifications
        .list(9, &query, &PageRequest::new(3, 2))
        .await
        .expect("page 3");
    assert_eq!(last.content.len(), 1);
    assert_eq!(last.content[0].kind, "kind0");
    assert!(!last.has_next);
}

#[tokio::test]
async fn list_filters_by_kind() {
    let notifications = service();
    notifications
        .notify(4, "mention", json!({}))
        .await
        .expect("notify");
    notifications
        .notify(4, "like", json!({}))
        .await
        .expect("notify");

    let query = ListQuery::new(None, SortDir::default(), &[("kind", "mention")]);
    let page = PageRequest::default();
    let feed = notifications.list(4, &query, &page).await.expect("list");
    assert_eq!(feed.total_elements, 1);
    assert_eq!(feed.content[0].kind, "mention");
}

// ── Conventional per-recipient channel topic ──────────────────────────────

#[test]
fn topic_is_stable_per_recipient() {
    assert_eq!(Notifications::topic(7), "notifications:7");
}

// ── Extractor: end-to-end through TestApp (no DB → memory fallback) ───────

mod extractor {
    use super::*;
    use autumn_web::prelude::*;
    use autumn_web::test::TestApp;

    #[post("/boop/{rid}")]
    async fn notify_route(
        Path(rid): Path<i64>,
        notifications: Notifications,
    ) -> AutumnResult<Json<Notification>> {
        let n = notifications
            .notify(rid, "boop", json!({"via": "route"}))
            .await?;
        Ok(Json(n))
    }

    #[get("/feed/{rid}")]
    async fn feed_route(
        Path(rid): Path<i64>,
        query: ListQuery,
        page: PageRequest,
        notifications: Notifications,
    ) -> AutumnResult<Json<Page<Notification>>> {
        Ok(Json(notifications.list(rid, &query, &page).await?))
    }

    #[get("/unread/{rid}")]
    async fn unread_route(
        Path(rid): Path<i64>,
        notifications: Notifications,
    ) -> AutumnResult<Json<u64>> {
        Ok(Json(notifications.unread_count(rid).await?))
    }

    #[post("/read/{rid}/{id}")]
    async fn mark_read_route(
        Path((rid, id)): Path<(i64, i64)>,
        notifications: Notifications,
    ) -> AutumnResult<&'static str> {
        notifications.mark_read_for(rid, id).await?;
        Ok("ok")
    }

    #[tokio::test]
    async fn notify_list_mark_read_flow_works_through_routes() {
        let client = TestApp::new()
            .routes(routes![
                notify_route,
                feed_route,
                unread_route,
                mark_read_route
            ])
            .build();

        // notify
        let created: Notification = client.post("/boop/7").send().await.assert_ok().json();
        assert_eq!(created.recipient_id, 7);

        // list
        let feed: Page<Notification> = client.get("/feed/7").send().await.assert_ok().json();
        assert_eq!(feed.total_elements, 1);

        // unread count
        let unread: u64 = client.get("/unread/7").send().await.assert_ok().json();
        assert_eq!(unread, 1);

        // mark read, then the unread feed is empty
        client
            .post(&format!("/read/7/{}", created.id))
            .send()
            .await
            .assert_ok();
        let unread: u64 = client.get("/unread/7").send().await.assert_ok().json();
        assert_eq!(unread, 0);

        let unread_feed: Page<Notification> = client
            .get("/feed/7?filter[unread]=true")
            .send()
            .await
            .assert_ok()
            .json();
        assert_eq!(unread_feed.total_elements, 0);
    }

    #[tokio::test]
    async fn memory_store_is_shared_across_requests_within_an_app() {
        let client = TestApp::new()
            .routes(routes![notify_route, unread_route])
            .build();
        client.post("/boop/1").send().await.assert_ok();
        client.post("/boop/1").send().await.assert_ok();
        let unread: u64 = client.get("/unread/1").send().await.assert_ok().json();
        assert_eq!(unread, 2);
    }
}

// ── Best-effort realtime push over channels ───────────────────────────────

#[cfg(feature = "ws")]
mod push {
    use super::*;
    use autumn_web::prelude::*;
    use autumn_web::test::TestApp;

    #[post("/notify-push/{rid}")]
    async fn notify_push_route(
        Path(rid): Path<i64>,
        notifications: Notifications,
    ) -> AutumnResult<Json<Notification>> {
        let n = notifications
            .notify_with_push(rid, "push.test", json!({"hello": true}))
            .await?;
        Ok(Json(n))
    }

    #[get("/unread-push/{rid}")]
    async fn unread_route(
        Path(rid): Path<i64>,
        notifications: Notifications,
    ) -> AutumnResult<Json<u64>> {
        Ok(Json(notifications.unread_count(rid).await?))
    }

    /// AC: the broadcast is best-effort — publishing to a topic with **no
    /// subscribers** (which errors on the local backend) never fails `notify`.
    #[tokio::test]
    async fn channel_failure_never_fails_notify() {
        let client = TestApp::new()
            .routes(routes![notify_push_route, unread_route])
            .build();

        client.post("/notify-push/3").send().await.assert_ok();
        let unread: u64 = client.get("/unread-push/3").send().await.assert_ok().json();
        assert_eq!(unread, 1, "notification persisted despite dead channel");
    }

    /// The push goes out on the conventional per-recipient topic with the
    /// serialized notification as payload.
    #[tokio::test]
    async fn push_publishes_notification_json_on_recipient_topic() {
        let client = TestApp::new().routes(routes![notify_push_route]).build();

        let mut rx = client
            .state()
            .channels()
            .subscribe(&Notifications::topic(3));

        let created: Notification = client
            .post("/notify-push/3")
            .send()
            .await
            .assert_ok()
            .json();

        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timely broadcast")
            .expect("broadcast delivered");
        let pushed: Notification =
            serde_json::from_str(&msg.into_string()).expect("payload is the notification JSON");
        assert_eq!(pushed.id, created.id);
        assert_eq!(pushed.kind, "push.test");
    }
}

// ── Review-pass hardening tests ───────────────────────────────────────────

/// A custom store registered on the builder pre-empts the default
/// resolution and receives every service call made through the extractor.
mod custom_store_registration {
    use super::*;
    use autumn_web::notifications::{NotificationStore, NotificationStoreError};
    use autumn_web::prelude::*;
    use autumn_web::test::TestApp;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Delegates to a memory store while counting notify calls, proving the
    /// registered instance (not a lazily created default) serves requests.
    struct CountingStore {
        inner: MemoryNotificationStore,
        notifies: Arc<AtomicUsize>,
    }

    impl NotificationStore for CountingStore {
        async fn notify(
            &self,
            recipient_id: i64,
            kind: String,
            payload: serde_json::Value,
        ) -> Result<Notification, NotificationStoreError> {
            self.notifies.fetch_add(1, Ordering::SeqCst);
            self.inner.notify(recipient_id, kind, payload).await
        }
        async fn list(
            &self,
            recipient_id: i64,
            query: &ListQuery,
            page: &PageRequest,
        ) -> Result<Page<Notification>, NotificationStoreError> {
            self.inner.list(recipient_id, query, page).await
        }
        async fn unread_count(&self, recipient_id: i64) -> Result<u64, NotificationStoreError> {
            self.inner.unread_count(recipient_id).await
        }
        async fn mark_read(
            &self,
            id: i64,
            recipient_id: Option<i64>,
        ) -> Result<u64, NotificationStoreError> {
            self.inner.mark_read(id, recipient_id).await
        }
        async fn mark_all_read(&self, recipient_id: i64) -> Result<u64, NotificationStoreError> {
            self.inner.mark_all_read(recipient_id).await
        }
    }

    #[post("/custom-notify/{rid}")]
    async fn notify_route(
        Path(rid): Path<i64>,
        notifications: Notifications,
    ) -> AutumnResult<Json<Notification>> {
        Ok(Json(notifications.notify(rid, "custom", json!({})).await?))
    }

    #[tokio::test]
    async fn with_notification_store_preempts_the_default_resolution() {
        let notifies = Arc::new(AtomicUsize::new(0));
        let client = TestApp::new()
            .routes(routes![notify_route])
            .with_notification_store(CountingStore {
                inner: MemoryNotificationStore::new(),
                notifies: Arc::clone(&notifies),
            })
            .build();

        client.post("/custom-notify/1").send().await.assert_ok();
        client.post("/custom-notify/1").send().await.assert_ok();

        assert_eq!(
            notifies.load(Ordering::SeqCst),
            2,
            "the registered store served the extractor's calls"
        );
    }
}

#[tokio::test]
async fn explicit_created_at_sort_returns_oldest_first_with_id_tiebreak() {
    let notifications = service();
    for i in 0..3 {
        notifications
            .notify(6, format!("k{i}"), json!({}))
            .await
            .expect("notify");
    }

    let oldest_first = ListQuery::new(Some("created_at"), SortDir::Asc, &[]);
    let feed = notifications
        .list(6, &oldest_first, &PageRequest::default())
        .await
        .expect("list");
    assert_eq!(
        feed.content
            .iter()
            .map(|n| n.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["k0", "k1", "k2"],
        "ascending created_at with monotonic-id tiebreak"
    );
}

#[tokio::test]
async fn concurrent_notify_assigns_unique_ids() {
    let notifications = service();
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let notifications = notifications.clone();
            tokio::spawn(async move { notifications.notify(1, format!("k{i}"), json!({})).await })
        })
        .collect();

    let mut ids = std::collections::HashSet::new();
    for handle in handles {
        let n = handle.await.expect("join").expect("notify");
        assert!(ids.insert(n.id), "duplicate id {}", n.id);
    }
    assert_eq!(ids.len(), 10);
    assert_eq!(notifications.unread_count(1).await.expect("count"), 10);
}

/// `notify_with_push` outside a request context (no channel registry) skips
/// the push entirely and still persists.
#[cfg(feature = "ws")]
#[tokio::test]
async fn notify_with_push_without_channels_still_persists() {
    let notifications = service();
    let n = notifications
        .notify_with_push(2, "quiet", json!({}))
        .await
        .expect("notify_with_push without channels");
    assert_eq!(n.kind, "quiet");
    assert_eq!(notifications.unread_count(2).await.expect("count"), 1);
}

// ── Postgres-backed store (testcontainers, requires Docker) ───────────────

// `not(feature = "sqlite")`: under the app-only `sqlite` feature (which
// implies `db`) the runtime connection is SQLite, so this Postgres pool
// would no longer type-check against `DbNotificationStore::new`.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
mod pg {
    use super::*;
    use autumn_web::notifications::DbNotificationStore;
    use diesel_async::AsyncPgConnection;
    use diesel_async::RunQueryDsl;
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    /// Matches the DDL emitted by `autumn generate notifications` for the
    /// Postgres backend (autumn-cli `generate/notifications.rs`). Keep the
    /// two in sync.
    const CREATE_NOTIFICATIONS_SQL: &str = "CREATE TABLE notifications (\
         id BIGSERIAL PRIMARY KEY, \
         recipient_id BIGINT NOT NULL, \
         kind TEXT NOT NULL, \
         payload TEXT NOT NULL, \
         read_at TIMESTAMPTZ, \
         created_at TIMESTAMPTZ NOT NULL DEFAULT NOW())";

    async fn setup() -> (
        Notifications,
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
        diesel::sql_query(CREATE_NOTIFICATIONS_SQL)
            .execute(&mut conn)
            .await
            .expect("create notifications table");
        drop(conn);

        (
            Notifications::new(DbNotificationStore::new(pool.clone())),
            pool,
            container,
        )
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn db_store_supports_the_full_feed_lifecycle() {
        let (notifications, _pool, _container) = setup().await;

        // Write path: notify for a recipient with no prior notifications.
        let a = notifications
            .notify(1, "comment.created", json!({"post": 1}))
            .await
            .expect("notify a");
        let b = notifications
            .notify(1, "like", json!({}))
            .await
            .expect("notify b");
        notifications
            .notify(2, "other", json!({}))
            .await
            .expect("notify other");

        // Read path: immediately visible.
        let (query, page) = default_list();
        let feed = notifications.list(1, &query, &page).await.expect("list");
        assert_eq!(feed.total_elements, 2);
        assert_eq!(feed.content[0].id, b.id, "newest first");
        assert_eq!(feed.content[0].payload, json!({}));
        assert_eq!(feed.content[1].payload, json!({"post": 1}));
        assert_eq!(notifications.unread_count(1).await.expect("count"), 2);

        // mark_read is idempotent and scoped correctly.
        notifications.mark_read(a.id).await.expect("mark_read");
        notifications
            .mark_read(a.id)
            .await
            .expect("mark_read again");
        assert_eq!(notifications.unread_count(1).await.expect("count"), 1);

        let unread_query = ListQuery::new(None, SortDir::default(), &[("unread", "true")]);
        let unread = notifications
            .list(1, &unread_query, &page)
            .await
            .expect("unread list");
        assert_eq!(unread.total_elements, 1);
        assert_eq!(unread.content[0].id, b.id);

        // mark_all_read is idempotent and per-recipient.
        assert_eq!(notifications.mark_all_read(1).await.expect("mark_all"), 1);
        assert_eq!(
            notifications
                .mark_all_read(1)
                .await
                .expect("mark_all again"),
            0
        );
        assert_eq!(notifications.unread_count(1).await.expect("count"), 0);
        assert_eq!(
            notifications
                .unread_count(2)
                .await
                .expect("other recipient count"),
            1,
            "other recipients' unread state is untouched"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn db_store_mark_read_for_is_recipient_scoped() {
        let (notifications, _pool, _container) = setup().await;
        let n = notifications
            .notify(1, "a", json!({}))
            .await
            .expect("notify");

        notifications
            .mark_read_for(2, n.id)
            .await
            .expect("foreign no-op");
        assert_eq!(notifications.unread_count(1).await.expect("count"), 1);

        notifications.mark_read_for(1, n.id).await.expect("owner");
        assert_eq!(notifications.unread_count(1).await.expect("count"), 0);
    }

    /// The `kind` filter, combined `unread`+`kind` filters, and explicit
    /// `sort=created_at` ordering on the DB store.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn db_store_filters_and_created_at_sort() {
        let (notifications, _pool, _container) = setup().await;
        let a = notifications
            .notify(1, "mention", json!({}))
            .await
            .expect("a");
        let b = notifications.notify(1, "like", json!({})).await.expect("b");
        let c = notifications
            .notify(1, "mention", json!({}))
            .await
            .expect("c");
        notifications.mark_read_for(1, a.id).await.expect("read a");

        let page = PageRequest::default();
        let kind_only = ListQuery::new(None, SortDir::default(), &[("kind", "mention")]);
        let feed = notifications
            .list(1, &kind_only, &page)
            .await
            .expect("kind");
        assert_eq!(feed.total_elements, 2);
        assert!(feed.content.iter().all(|n| n.kind == "mention"));

        let combined = ListQuery::new(
            None,
            SortDir::default(),
            &[("kind", "mention"), ("unread", "true")],
        );
        let feed = notifications
            .list(1, &combined, &page)
            .await
            .expect("combined");
        assert_eq!(feed.total_elements, 1);
        assert_eq!(feed.content[0].id, c.id);

        let oldest_first = ListQuery::new(Some("created_at"), SortDir::Asc, &[]);
        let feed = notifications
            .list(1, &oldest_first, &page)
            .await
            .expect("created_at asc");
        assert_eq!(
            feed.content.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![a.id, b.id, c.id],
            "oldest first with id tiebreak"
        );
    }

    /// A payload corrupted out of band degrades to a JSON string of the raw
    /// text instead of failing the whole feed.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn db_store_degrades_unparseable_payload_to_string() {
        let (notifications, pool, _container) = setup().await;
        let n = notifications
            .notify(1, "k", json!({"ok": true}))
            .await
            .expect("notify");

        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query("UPDATE notifications SET payload = 'not json' WHERE id = $1")
            .bind::<diesel::sql_types::BigInt, _>(n.id)
            .execute(&mut conn)
            .await
            .expect("corrupt payload");
        drop(conn);

        let (query, page) = default_list();
        let feed = notifications.list(1, &query, &page).await.expect("list");
        assert_eq!(feed.content[0].payload, json!("not json"));
    }

    /// With a DB pool configured, the `Notifications` extractor auto-selects
    /// the database-backed store (not the memory fallback): rows written
    /// through a route land in the `notifications` table.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn extractor_auto_selects_db_store_when_pool_is_configured() {
        use autumn_web::prelude::*;
        use autumn_web::test::TestApp;

        #[post("/pg-notify/{rid}")]
        async fn notify_route(
            Path(rid): Path<i64>,
            notifications: Notifications,
        ) -> AutumnResult<Json<Notification>> {
            Ok(Json(notifications.notify(rid, "via-db", json!({})).await?))
        }

        #[derive(diesel::QueryableByName)]
        struct CountRow {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            n: i64,
        }

        let (_notifications, pool, _container) = setup().await;
        let client = TestApp::new()
            .routes(routes![notify_route])
            .with_db(pool.clone())
            .build();

        client.post("/pg-notify/42").send().await.assert_ok();

        let mut conn = pool.get().await.expect("conn");
        let row: CountRow =
            diesel::sql_query("SELECT COUNT(*) AS n FROM notifications WHERE recipient_id = 42")
                .get_result(&mut conn)
                .await
                .expect("count");
        assert_eq!(
            row.n, 1,
            "row persisted in the database, not the memory fallback"
        );
    }
}
