//! Integration tests for declarative auto-broadcasting.
//!
//! Run with:
//!
//!     cargo test --test auto_broadcast --features "ws,db,test-support"

#[cfg(all(feature = "db", feature = "ws", feature = "test-support"))]
mod tests {
    use autumn_web::prelude::*;
    use autumn_web::test::TestDb;
    use diesel_async::RunQueryDsl;

    // ── Schema ─────────────────────────────────────────────────

    diesel::table! {
        broadcast_posts (id) {
            id -> Int8,
            title -> Text,
        }
    }

    #[autumn_web::model(table = "broadcast_posts")]
    pub struct BroadcastPost {
        #[id]
        pub id: i64,
        pub title: String,
    }

    // A model with a custom render function.
    fn render_custom_post(post: &BroadcastPost) -> maud::Markup {
        maud::html! {
            div data-id="123" id=(format!("custom-post-{}", post.id)) {
                h2 { (post.title) }
            }
        }
    }

    #[cfg(all(feature = "htmx", feature = "maud"))]
    impl LiveFragment for BroadcastPost {
        fn dom_id_for(id: i64) -> String {
            format!("broadcast_post-{id}")
        }

        fn dom_id(&self) -> String {
            Self::dom_id_for(self.id)
        }

        fn render_fragment(&self) -> Markup {
            html! { li id=(self.dom_id()) { (self.title) } }
        }

        fn insert_swap() -> OobSwap {
            OobSwap::Target(
                autumn_web::htmx::OobMethod::BeforeEnd,
                "#broadcast_posts-list".to_string(),
            )
        }
    }

    // 1. Basic auto-broadcasting (defaults to topic = "broadcast_posts", container = "broadcast_posts-list")
    #[autumn_web::repository(BroadcastPost, table = "broadcast_posts", broadcasts = true)]
    pub trait BasicPostRepository {}

    // 2. Custom broadcasting (custom topic with field interpolation, custom container, custom render)
    #[autumn_web::repository(
        BroadcastPost,
        table = "broadcast_posts",
        broadcasts = true,
        topic = "post_topic:{title}",
        render = render_custom_post,
        container = "custom-posts-list"
    )]
    pub trait CustomPostRepository {}

    // 3. Nullable model with a category (Option<String>)
    diesel::table! {
        nullable_posts (id) {
            id -> Int8,
            title -> Text,
            category -> Nullable<Text>,
        }
    }

    #[autumn_web::model(table = "nullable_posts")]
    pub struct NullablePost {
        #[id]
        pub id: i64,
        pub title: String,
        pub category: Option<String>,
    }

    #[cfg(all(feature = "htmx", feature = "maud"))]
    impl LiveFragment for NullablePost {
        fn dom_id_for(id: i64) -> String {
            format!("nullable_post-{id}")
        }

        fn dom_id(&self) -> String {
            Self::dom_id_for(self.id)
        }

        fn render_fragment(&self) -> Markup {
            html! { li id=(self.dom_id()) { (self.title) } }
        }

        fn insert_swap() -> OobSwap {
            OobSwap::Target(
                autumn_web::htmx::OobMethod::BeforeEnd,
                "#category-posts-list".to_string(),
            )
        }
    }

    #[autumn_web::repository(
        NullablePost,
        table = "nullable_posts",
        broadcasts = true,
        topic = "category_posts:{category}",
        container = "category-posts-list"
    )]
    pub trait NullablePostRepository {}

    // ── Setup ──────────────────────────────────────────────────

    async fn setup_db(db: &TestDb) {
        let mut conn = db.pool().get().await.expect("db connection");
        diesel::sql_query(
            "CREATE TABLE IF NOT EXISTS broadcast_posts (
                id BIGSERIAL PRIMARY KEY,
                title TEXT NOT NULL
            )",
        )
        .execute(&mut *conn)
        .await
        .expect("create broadcast_posts table");

        diesel::sql_query(
            "CREATE TABLE IF NOT EXISTS nullable_posts (
                id BIGSERIAL PRIMARY KEY,
                title TEXT NOT NULL,
                category TEXT
            )",
        )
        .execute(&mut *conn)
        .await
        .expect("create nullable_posts table");

        diesel::sql_query(
            "CREATE TABLE IF NOT EXISTS autumn_repository_commit_hooks (
                id TEXT PRIMARY KEY,
                handler_key TEXT NOT NULL,
                hook_name TEXT NOT NULL,
                context JSONB NOT NULL,
                record JSONB NOT NULL,
                status TEXT NOT NULL,
                attempt INTEGER NOT NULL,
                max_attempts INTEGER NOT NULL,
                initial_backoff_ms BIGINT NOT NULL,
                enqueued_at TIMESTAMP NOT NULL,
                run_at TIMESTAMP NOT NULL,
                claimed_by TEXT,
                claimed_at TIMESTAMP,
                started_at TIMESTAMP,
                finished_at TIMESTAMP,
                last_error TEXT
            )",
        )
        .execute(&mut *conn)
        .await
        .expect("create autumn_repository_commit_hooks table");
    }

    // ── Tests ──────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    #[allow(clippy::too_many_lines)]
    async fn test_auto_broadcast_lifecycle() {
        let _ = ::tracing_subscriber::fmt()
            .with_env_filter(::tracing_subscriber::EnvFilter::from_default_env())
            .try_init();
        let db = TestDb::shared().await;
        setup_db(db).await;

        let state = AppState::detached();
        let channels = state.channels().clone();
        // Register channels globally in the worker
        autumn_web::__private::set_global_channels(channels.clone());

        // Subscribe to basic and custom topics
        let mut basic_sub = channels.subscribe("broadcast_posts");
        let mut custom_sub = channels.subscribe("post_topic:hello");

        let repo = PgBasicPostRepository::with_pool_untracked(db.pool());
        let custom_repo = PgCustomPostRepository::with_pool_untracked(db.pool());

        // Start the background commit hook worker
        let shutdown = tokio_util::sync::CancellationToken::new();
        autumn_web::__private::start_repository_commit_hook_worker(
            db.pool(),
            Some(channels.clone()),
            shutdown.child_token(),
        );

        // 1. Create on basic repository
        let new_post = repo
            .save(&NewBroadcastPost {
                title: "hello".to_owned(),
            })
            .await
            .expect("save post");

        // Wait for the background worker to drain the hook and publish
        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), basic_sub.recv())
            .await
            .expect("timeout waiting for basic create broadcast")
            .expect("recv error");
        let html_content = msg.into_string();
        assert!(html_content.contains("hx-swap-oob=\"beforeend:#broadcast_posts-list\""));
        assert!(html_content.contains(&format!("broadcast_post-{}", new_post.id)));

        // 2. Create on custom repository
        let custom_post = custom_repo
            .save(&NewBroadcastPost {
                title: "hello".to_owned(),
            })
            .await
            .expect("save custom post");

        // Wait for the background worker to drain and publish on custom topic (interpolated post_topic:hello)
        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), custom_sub.recv())
            .await
            .expect("timeout waiting for custom create broadcast")
            .expect("recv error");
        let html_content = msg.into_string();
        assert!(html_content.contains("hx-swap-oob=\"beforeend:#custom-posts-list\""));
        assert!(html_content.contains(&format!("id=\"custom-post-{}", custom_post.id)));
        assert!(html_content.contains("hello"));

        // 3. Update on custom repository (which changes the topic from post_topic:hello to post_topic:world)
        let mut old_topic_sub = channels.subscribe("post_topic:hello");
        let mut custom_update_sub = channels.subscribe("post_topic:world");

        let update_changes = UpdateBroadcastPost {
            title: autumn_web::hooks::Patch::Set("world".to_owned()),
        };
        custom_repo
            .update(custom_post.id, &update_changes)
            .await
            .expect("update custom post");

        // Wait for the background worker to drain and publish delete on the old topic
        let msg_old = tokio::time::timeout(std::time::Duration::from_secs(3), old_topic_sub.recv())
            .await
            .expect("timeout waiting for delete on old topic broadcast")
            .expect("recv error");
        let html_content_old = msg_old.into_string();
        assert!(html_content_old.contains(&format!(
            "hx-swap-oob=\"delete:#custom-post-{}\"",
            custom_post.id
        )));

        // Wait for the background worker to drain and publish update on custom topic
        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), custom_update_sub.recv())
            .await
            .expect("timeout waiting for custom update broadcast")
            .expect("recv error");
        let html_content = msg.into_string();
        assert!(html_content.contains("hx-swap-oob=\"beforeend:#custom-posts-list\""));
        assert!(html_content.contains(&format!("id=\"custom-post-{}", custom_post.id)));
        assert!(html_content.contains("world"));

        // 4. Delete on custom repository
        custom_repo
            .delete_by_id(custom_post.id)
            .await
            .expect("delete custom post");

        // Wait for the background worker to drain and publish delete on custom topic
        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), custom_update_sub.recv())
            .await
            .expect("timeout waiting for custom delete broadcast")
            .expect("recv error");
        let html_content = msg.into_string();
        assert!(html_content.contains(&format!(
            "hx-swap-oob=\"delete:#custom-post-{}\"",
            custom_post.id
        )));

        // 5. Test bulk update (update_many) and nullable topic placeholders
        let nullable_repo = PgNullablePostRepository::with_pool_untracked(db.pool());

        let post_none = nullable_repo
            .save(&NewNullablePost {
                title: "post_none".to_owned(),
                category: None,
            })
            .await
            .expect("save post_none");

        let post_some = nullable_repo
            .save(&NewNullablePost {
                title: "post_some".to_owned(),
                category: Some("rust".to_owned()),
            })
            .await
            .expect("save post_some");

        // We subscribe to the topics
        let mut none_sub = channels.subscribe("category_posts:none");
        let mut rust_sub = channels.subscribe("category_posts:rust");
        let mut go_sub = channels.subscribe("category_posts:go");

        // Drain any initial creation broadcasts
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), none_sub.recv()).await;
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), rust_sub.recv()).await;

        // Perform a bulk update changing category from None -> Some("go") and Some("rust") -> Some("go")
        let update_changes = UpdateNullablePost {
            title: autumn_web::hooks::Patch::Unchanged,
            category: autumn_web::hooks::Patch::Set(Some("go".to_owned())),
        };
        nullable_repo
            .update_many(&[post_none.id, post_some.id], &update_changes)
            .await
            .expect("update_many posts");

        // Wait for OOB deletes on the old topics:
        // - category_posts:none should receive a delete for post_none.id
        let msg_none = tokio::time::timeout(std::time::Duration::from_secs(3), none_sub.recv())
            .await
            .expect("timeout waiting for delete on none topic")
            .expect("recv error");
        assert!(msg_none.into_string().contains(&format!(
            "hx-swap-oob=\"delete:#nullable_post-{}\"",
            post_none.id
        )));

        // - category_posts:rust should receive a delete for post_some.id
        let msg_rust = tokio::time::timeout(std::time::Duration::from_secs(3), rust_sub.recv())
            .await
            .expect("timeout waiting for delete on rust topic")
            .expect("recv error");
        assert!(msg_rust.into_string().contains(&format!(
            "hx-swap-oob=\"delete:#nullable_post-{}\"",
            post_some.id
        )));

        // - category_posts:go should receive updates for both
        let msg_go1 = tokio::time::timeout(std::time::Duration::from_secs(3), go_sub.recv())
            .await
            .expect("timeout waiting for go topic update 1")
            .expect("recv error")
            .into_string();
        let msg_go2 = tokio::time::timeout(std::time::Duration::from_secs(3), go_sub.recv())
            .await
            .expect("timeout waiting for go topic update 2")
            .expect("recv error")
            .into_string();

        let combined = format!("{msg_go1} {msg_go2}");
        assert!(combined.contains("hx-swap-oob=\"beforeend:#category-posts-list\""));
        assert!(combined.contains(&format!("nullable_post-{}", post_none.id)));
        assert!(combined.contains(&format!("nullable_post-{}", post_some.id)));

        shutdown.cancel();
    }
}
